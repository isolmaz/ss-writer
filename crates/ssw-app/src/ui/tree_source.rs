//! The sidebar's [`TreeSource`] over [`ssw_io::workspace::Workspace`].
//!
//! The widget identifies entries by a `u64` key; the app needs the path behind a key (open the
//! file, rename, delete). Keys are indexes into a shared [`TreePaths`] table that the source
//! fills while listing folders and the controller reads.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ssw_io::fs::{FileSystem, FsError};
use ssw_io::watch::{FsEvent, FsEventKind};
use ssw_io::workspace::{FileKind, Node, SortKey as IoSortKey, ViewOptions, Workspace};
use ssw_ui::shell::sidebar::{
    EntryKind, NodeKey, SidebarEntry, SidebarOptions, SortKey as UiSortKey, TreeSource,
};

use crate::documents::{DocMode, mode_for_path};

/// Key of the workspace root.
const ROOT_KEY: NodeKey = 0;

/// The paths behind the keys handed to the sidebar. Shared between the [`WorkspaceTree`] inside
/// the widget and the controller.
#[derive(Debug, Clone, Default)]
pub struct TreePaths {
    inner: Rc<RefCell<Table>>,
}

#[derive(Debug, Default)]
struct Table {
    paths: Vec<PathBuf>,
    keys: HashMap<PathBuf, NodeKey>,
    /// Changes on disk the tree has not looked at yet (see [`TreePaths::queue_event`]).
    pending: Vec<FsEvent>,
}

impl TreePaths {
    /// The path of entry `key`.
    #[must_use]
    pub fn path(&self, key: NodeKey) -> Option<PathBuf> {
        // A `u64` key always fits a `usize` on Windows (x64).
        self.inner.borrow().paths.get(key as usize).cloned()
    }

    /// The key of `path`, handing out a new one the first time.
    pub(super) fn key(&self, path: &Path) -> NodeKey {
        let mut table = self.inner.borrow_mut();
        if let Some(&key) = table.keys.get(path) {
            return key;
        }
        let key = table.paths.len() as NodeKey;
        table.paths.push(path.to_path_buf());
        table.keys.insert(path.to_path_buf(), key);
        key
    }

    /// Tells the tree that something changed on disk. The change shows the next time the
    /// sidebar lists a folder (`SidebarView::refresh_all`): the tree applies every queued event
    /// to its loaded folders first, so the listing needs no new disk read of its own.
    pub fn queue_event(&self, event: FsEvent) {
        self.inner.borrow_mut().pending.push(event);
    }

    /// Queues a full re-read of every loaded folder (after the app itself renamed, moved or
    /// created files).
    pub fn request_rescan(&self) {
        if let Some(root) = self.path(ROOT_KEY) {
            self.queue_event(FsEvent {
                path: root,
                kind: FsEventKind::Rescan,
            });
        }
    }

    fn take_events(&self) -> Vec<FsEvent> {
        std::mem::take(&mut self.inner.borrow_mut().pending)
    }
}

/// A [`TreeSource`] listing the folders of one workspace root, lazily.
#[derive(Debug)]
pub struct WorkspaceTree {
    workspace: Workspace,
    paths: TreePaths,
}

impl WorkspaceTree {
    /// Opens `root`; the first level is loaded.
    ///
    /// # Errors
    /// [`FsError::NotFound`] when the folder does not exist, or the listing error.
    pub fn open(fs: Arc<dyn FileSystem>, root: &Path) -> Result<Self, FsError> {
        let workspace = Workspace::open(fs, root)?;
        let paths = TreePaths::default();
        let key = paths.key(workspace.root());
        debug_assert_eq!(key, ROOT_KEY);
        Ok(Self { workspace, paths })
    }

    /// The table that maps the widget's keys back to paths.
    #[must_use]
    pub fn paths(&self) -> TreePaths {
        self.paths.clone()
    }
}

fn view_options(options: &SidebarOptions) -> ViewOptions {
    ViewOptions {
        sort: match options.sort {
            UiSortKey::Name => IoSortKey::Name,
            UiSortKey::Modified => IoSortKey::Modified,
            UiSortKey::Created => IoSortKey::Created,
            UiSortKey::Kind => IoSortKey::Kind,
        },
        descending: options.descending,
        folders_first: options.folders_first,
        show_hidden: options.show_hidden,
        hide_images: options.hide_images,
        hide_non_markdown: options.hide_non_markdown,
    }
}

/// The icon class of a file. The word-processor files and TextBundle packs the app imports as
/// Markdown open in the editor, so they are prose: `ssw_io` classifies by text type only (they
/// land among its code files, like every unknown type) and leaves them to [`mode_for_path`],
/// which owns the list of what the editor opens as Markdown. Only the types `ssw_io` knows to
/// open in another app are [`EntryKind::Other`], which the tree dims.
fn entry_kind(node: &Node) -> EntryKind {
    match node.kind() {
        FileKind::Prose => EntryKind::Prose,
        FileKind::Code if mode_for_path(Path::new(&node.name)) == DocMode::Markdown => {
            EntryKind::Prose
        }
        FileKind::Code => EntryKind::Code,
        FileKind::Image => EntryKind::Image,
        FileKind::Other => EntryKind::Other,
    }
}

fn entry_for(key: NodeKey, node: &Node) -> SidebarEntry {
    let mut entry = if node.is_dir() {
        SidebarEntry::folder(key, node.name.clone())
    } else {
        SidebarEntry::file(key, node.name.clone(), entry_kind(node))
    };
    entry.placeholder = node.placeholder;
    entry.hidden = node.is_hidden();
    entry
}

impl TreeSource for WorkspaceTree {
    fn root_name(&self) -> String {
        self.workspace.tree().name.clone()
    }

    fn root_key(&self) -> NodeKey {
        ROOT_KEY
    }

    fn children(&mut self, folder: NodeKey, options: &SidebarOptions) -> Vec<SidebarEntry> {
        for event in self.paths.take_events() {
            // A folder that cannot be re-read keeps its old listing; the next change retries.
            let _ = self.workspace.apply(&event);
        }
        let Some(path) = self.paths.path(folder) else {
            return Vec::new();
        };
        // An unreadable folder lists as empty (the trait's contract).
        if self.workspace.load_children(&path).is_err() {
            return Vec::new();
        }
        let view = view_options(options);
        self.workspace
            .visible_children(&path, &view)
            .into_iter()
            .map(|node| entry_for(self.paths.key(&path.join(&node.name)), node))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ssw_io::fs::RealFs;

    use super::*;

    #[test]
    fn lists_folders_lazily_with_markdown_first_class_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("notes")).unwrap();
        std::fs::write(tmp.path().join("notes").join("deep.md"), "x").unwrap();
        std::fs::write(tmp.path().join("a.md"), "x").unwrap();
        std::fs::write(tmp.path().join("pic.png"), "x").unwrap();
        std::fs::write(tmp.path().join("b.bin"), "x").unwrap();
        let mut tree = WorkspaceTree::open(Arc::new(RealFs), tmp.path()).unwrap();
        let paths = tree.paths();
        assert_eq!(paths.path(tree.root_key()).unwrap(), tmp.path());
        let top = tree.children(tree.root_key(), &SidebarOptions::default());
        let names: Vec<_> = top.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["notes", "a.md", "b.bin", "pic.png"]);
        assert_eq!(top[0].kind, EntryKind::Folder);
        assert_eq!(top[1].kind, EntryKind::Prose);
        assert_eq!(top[2].kind, EntryKind::Other);
        assert_eq!(top[3].kind, EntryKind::Image);
        assert_eq!(paths.path(top[1].key).unwrap(), tmp.path().join("a.md"));
        let inner = tree.children(top[0].key, &SidebarOptions::default());
        assert_eq!(inner.len(), 1);
        assert_eq!(
            paths.path(inner[0].key).unwrap(),
            tmp.path().join("notes").join("deep.md")
        );
        // The same path always gets the same key.
        let again = tree.children(top[0].key, &SidebarOptions::default());
        assert_eq!(again[0].key, inner[0].key);
        // Filters reach the listing.
        let options = SidebarOptions {
            hide_non_markdown: true,
            ..SidebarOptions::default()
        };
        let filtered = tree.children(tree.root_key(), &options);
        assert_eq!(
            filtered
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["notes", "a.md"]
        );
        assert!(tree.children(9999, &options).is_empty());
        assert_eq!(paths.path(9999), None);
    }

    #[test]
    fn files_the_editor_opens_are_full_strength_and_only_known_foreign_types_are_dimmed() {
        let tmp = tempfile::tempdir().unwrap();
        for name in [
            "a.md",
            "b.txt",
            "Makefile",
            "c.py",
            "d.json",
            "e.html",
            "E.JSON",
            "f.docx",
            "g.odt",
            "h.rtf",
            "i.doc",
            "j.textpack",
            "k.png",
            "l.zip",
            "m.exe",
            "n.xyz",
            "p.kt",
            "q.vue",
            "r.lua",
            "s.swift",
            "Procfile",
            ".gitignore",
            "t.pdf",
            "u.MP4",
        ] {
            std::fs::write(tmp.path().join(name), "x").unwrap();
        }
        std::fs::create_dir(tmp.path().join("o.textbundle")).unwrap();
        let mut tree = WorkspaceTree::open(Arc::new(RealFs), tmp.path()).unwrap();
        let entries = tree.children(tree.root_key(), &SidebarOptions::default());
        let entry_of = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .expect("the file is listed")
        };
        for (name, kind, dimmed) in [
            ("a.md", EntryKind::Prose, false),
            ("b.txt", EntryKind::Prose, false),
            ("Makefile", EntryKind::Prose, false),
            ("c.py", EntryKind::Code, false),
            ("d.json", EntryKind::Code, false),
            ("e.html", EntryKind::Code, false),
            ("E.JSON", EntryKind::Code, false),
            ("f.docx", EntryKind::Prose, false),
            ("g.odt", EntryKind::Prose, false),
            ("h.rtf", EntryKind::Prose, false),
            ("i.doc", EntryKind::Prose, false),
            ("j.textpack", EntryKind::Prose, false),
            ("k.png", EntryKind::Image, true),
            ("l.zip", EntryKind::Other, true),
            ("m.exe", EntryKind::Other, true),
            ("n.xyz", EntryKind::Code, false),
            ("p.kt", EntryKind::Code, false),
            ("q.vue", EntryKind::Code, false),
            ("r.lua", EntryKind::Code, false),
            ("s.swift", EntryKind::Code, false),
            ("Procfile", EntryKind::Prose, false),
            (".gitignore", EntryKind::Prose, false),
            ("t.pdf", EntryKind::Other, true),
            ("u.MP4", EntryKind::Other, true),
            ("o.textbundle", EntryKind::Folder, false),
        ] {
            let entry = entry_of(name);
            assert_eq!(entry.kind, kind, "{name}");
            assert_eq!(entry.is_dimmed(), dimmed, "{name}");
        }
    }

    #[test]
    fn every_sort_key_and_filter_of_the_sidebar_reaches_the_listing() {
        for (sort, expected) in [
            (UiSortKey::Name, IoSortKey::Name),
            (UiSortKey::Modified, IoSortKey::Modified),
            (UiSortKey::Created, IoSortKey::Created),
            (UiSortKey::Kind, IoSortKey::Kind),
        ] {
            let options = SidebarOptions {
                sort,
                descending: true,
                folders_first: false,
                show_hidden: true,
                hide_images: true,
                hide_non_markdown: true,
                ..SidebarOptions::default()
            };
            assert_eq!(
                view_options(&options),
                ViewOptions {
                    sort: expected,
                    descending: true,
                    folders_first: false,
                    show_hidden: true,
                    hide_images: true,
                    hide_non_markdown: true,
                }
            );
        }
    }

    #[test]
    fn a_rescan_is_queued_for_the_root_of_an_opened_tree_and_not_for_an_empty_table() {
        let paths = TreePaths::default();
        paths.request_rescan();
        assert!(paths.take_events().is_empty(), "no root, nothing to rescan");

        let tmp = tempfile::tempdir().unwrap();
        let tree = WorkspaceTree::open(Arc::new(RealFs), tmp.path()).unwrap();
        let opened = tree.paths();
        opened.request_rescan();
        let events = opened.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, tmp.path());
        assert_eq!(format!("{:?}", events[0].kind), "Rescan");
        assert!(opened.take_events().is_empty(), "taken once");
    }

    #[test]
    fn a_missing_root_is_an_error_and_a_vanished_folder_lists_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let error = WorkspaceTree::open(Arc::new(RealFs), &missing).unwrap_err();
        assert_eq!(
            format!("{error:?}"),
            format!("{:?}", FsError::NotFound(missing))
        );
        std::fs::create_dir(tmp.path().join("gone")).unwrap();
        let mut tree = WorkspaceTree::open(Arc::new(RealFs), tmp.path()).unwrap();
        let top = tree.children(0, &SidebarOptions::default());
        std::fs::remove_dir(tmp.path().join("gone")).unwrap();
        assert!(
            tree.children(top[0].key, &SidebarOptions::default())
                .is_empty()
        );
        assert_eq!(
            tree.root_name(),
            tmp.path().file_name().unwrap().to_string_lossy()
        );
    }
}
