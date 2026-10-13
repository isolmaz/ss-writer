use std::cmp::Ordering;
use std::os::windows::fs::FileTimesExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ssw_io::fs::{Attributes, FaultFs, FileSystem, FsError, FsFailure, FsOp, RealFs};
use ssw_io::watch::{FsEvent, FsEventKind};
use ssw_io::workspace::{
    DEFAULT_FOLDER_NAME, FileClass, Node, NodeKind, SortKey, UNTITLED_BASE_NAME, ViewOptions,
    WELCOME_FILE_NAME, Workspace, classify_extension, compare_logical, ensure_default_folder,
    known_documents_dir, opens_elsewhere, untitled_name,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn real() -> Arc<dyn FileSystem> {
    Arc::new(RealFs)
}

fn touch(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn set_times(path: &Path, created: u64, modified: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let times = std::fs::FileTimes::new()
        .set_modified(UNIX_EPOCH + Duration::from_secs(modified))
        .set_created(UNIX_EPOCH + Duration::from_secs(created));
    file.set_times(times).unwrap();
}

fn listing(ws: &Workspace, path: &Path, options: &ViewOptions) -> Vec<String> {
    ws.visible_children(path, options)
        .into_iter()
        .map(|node| node.name.clone())
        .collect()
}

fn event(path: &Path, kind: FsEventKind) -> FsEvent {
    FsEvent {
        path: path.to_path_buf(),
        kind,
    }
}

fn child_names(node: &Node) -> Vec<String> {
    let mut names: Vec<String> = node
        .children
        .as_ref()
        .expect("children loaded")
        .iter()
        .map(|child| child.name.clone())
        .collect();
    names.sort();
    names
}

#[test]
fn extensions_are_classified_case_insensitively() {
    let table = [
        ("md", FileClass::Markdown),
        ("MD", FileClass::Markdown),
        ("markdown", FileClass::Markdown),
        ("mdx", FileClass::Markdown),
        ("png", FileClass::Image),
        ("JPG", FileClass::Image),
        ("jpeg", FileClass::Image),
        ("gif", FileClass::Image),
        ("bmp", FileClass::Image),
        ("tif", FileClass::Image),
        ("tiff", FileClass::Image),
        ("webp", FileClass::Image),
        ("heic", FileClass::Image),
        ("ico", FileClass::Image),
        ("svg", FileClass::Image),
        ("txt", FileClass::Text),
        ("TXT", FileClass::Text),
        ("json", FileClass::Text),
        ("yml", FileClass::Text),
        ("rs", FileClass::Text),
        ("conf", FileClass::Text),
        ("exe", FileClass::Other),
        ("docx", FileClass::Other),
        ("", FileClass::Other),
    ];
    for (ext, expected) in table {
        assert_eq!(classify_extension(ext), expected, "extension {ext:?}");
    }
}

#[test]
fn every_text_extension_is_text_and_none_of_them_opens_elsewhere() {
    let text = [
        "txt", "text", "log", "csv", "tsv", "json", "jsonc", "yaml", "yml", "ini", "toml", "xml",
        "html", "htm", "css", "js", "ts", "tsx", "jsx", "py", "rs", "go", "java", "c", "h", "cpp",
        "cs", "sh", "bat", "ps1", "sql", "tex", "rst", "mmd", "cfg", "conf",
    ];
    for ext in text {
        assert_eq!(classify_extension(ext), FileClass::Text, "{ext}");
        assert!(!opens_elsewhere(ext), "{ext}");
    }
}

#[test]
fn only_the_listed_extensions_open_elsewhere_in_any_case() {
    let table = [
        ("pdf", true),
        ("PDF", true),
        ("Zip", true),
        ("exe", true),
        ("EPUB", true),
        // Pictures are classified apart (`FileClass::Image`), not listed.
        ("png", false),
        ("SVG", false),
        ("md", false),
        ("MD", false),
        ("txt", false),
        ("JSON", false),
        ("Py", false),
        // The app imports these itself: no list here claims them.
        ("docx", false),
        ("odt", false),
        ("rtf", false),
        ("doc", false),
        ("textpack", false),
        // What no list names is read as text by the editor: unknown types and a file without an
        // extension.
        ("kt", false),
        ("xyz", false),
        ("", false),
    ];
    for (ext, expected) in table {
        assert_eq!(opens_elsewhere(ext), expected, "extension {ext:?}");
    }
}

#[test]
fn every_extension_known_to_open_elsewhere_is_other() {
    let foreign = [
        "7z", "avi", "bin", "bz2", "class", "db", "dll", "dylib", "epub", "exe", "flac", "gz",
        "iso", "jar", "lib", "lnk", "m4a", "mkv", "mov", "mp3", "mp4", "msi", "o", "obj", "odp",
        "ods", "ogg", "otf", "pdb", "pdf", "ppt", "pptx", "pyc", "rar", "rlib", "rmeta", "so",
        "sqlite", "sys", "tar", "ttf", "wasm", "wav", "webm", "wmv", "woff", "woff2", "xls",
        "xlsx", "xz", "zip", "zst",
    ];
    assert_eq!(foreign.len(), 52);
    for ext in foreign {
        assert_eq!(classify_extension(ext), FileClass::Other, "{ext}");
        assert!(opens_elsewhere(ext), "{ext}");
        assert!(opens_elsewhere(&ext.to_uppercase()), "{ext} in capitals");
    }
}

#[test]
fn nodes_carry_what_the_disk_reports() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("Doc.MD"), "# hello");
    touch(&dir.path().join("noext"), "");
    touch(&dir.path().join(".gitignore"), "target");
    touch(&dir.path().join("archive.tar.gz"), "xyz");
    touch(&dir.path().join("Main.kt"), "fun main() {}");
    std::fs::create_dir(dir.path().join("sub.d")).unwrap();
    let ws = Workspace::open(real(), dir.path()).unwrap();

    assert_eq!(ws.root(), dir.path());
    let tree = ws.tree();
    assert_eq!(tree.name, dir.path().file_name().unwrap().to_string_lossy());
    assert!(tree.is_dir());
    assert_eq!(
        child_names(tree),
        [
            ".gitignore",
            "Doc.MD",
            "Main.kt",
            "archive.tar.gz",
            "noext",
            "sub.d"
        ]
    );

    let doc = ws.node(&dir.path().join("Doc.MD")).unwrap();
    assert_eq!(doc.kind, NodeKind::File);
    assert_eq!(doc.ext, "md");
    assert_eq!(doc.size, 7);
    assert_eq!(doc.class(), FileClass::Markdown);
    assert!(!opens_elsewhere(&doc.ext));
    assert!(doc.children.is_none());
    assert!(!doc.placeholder && !doc.is_hidden());
    assert!(doc.created.0 > 0 && doc.modified.0 > 0);

    // An extensionless file and a dotfile are read as text: nothing opens them elsewhere.
    let bare = ws.node(&dir.path().join("noext")).unwrap();
    assert_eq!(bare.ext, "");
    assert_eq!(bare.class(), FileClass::Other);
    assert!(!opens_elsewhere(&bare.ext));
    let dotfile = ws.node(&dir.path().join(".gitignore")).unwrap();
    assert_eq!(dotfile.ext, "");
    assert!(!opens_elsewhere(&dotfile.ext));
    let archive = ws.node(&dir.path().join("archive.tar.gz")).unwrap();
    assert_eq!(archive.ext, "gz");
    assert!(opens_elsewhere(&archive.ext));
    // A source file the lists do not know is read as text, so it is not listed either.
    let main = ws.node(&dir.path().join("Main.kt")).unwrap();
    assert_eq!(main.ext, "kt");
    assert_eq!(main.class(), FileClass::Other);
    assert!(!opens_elsewhere(&main.ext));
    let sub = ws.node(&dir.path().join("sub.d")).unwrap();
    assert_eq!(sub.kind, NodeKind::Dir);
    assert_eq!(sub.ext, "");
    assert_eq!(sub.size, 0);
    assert_eq!(sub.class(), FileClass::Other);
    assert!(sub.children.is_none());
}

#[test]
fn display_names_hide_extensions_on_request() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("Doc.MD"), "");
    touch(&dir.path().join(".gitignore"), "");
    touch(&dir.path().join("noext"), "");
    touch(&dir.path().join("archive.tar.gz"), "");
    std::fs::create_dir(dir.path().join("sub.d")).unwrap();
    let ws = Workspace::open(real(), dir.path()).unwrap();
    let table = [
        ("Doc.MD", "Doc", "Doc.MD"),
        (".gitignore", ".gitignore", ".gitignore"),
        ("noext", "noext", "noext"),
        ("archive.tar.gz", "archive.tar", "archive.tar.gz"),
        ("sub.d", "sub.d", "sub.d"),
    ];
    for (name, hidden, shown) in table {
        let node = ws.node(&dir.path().join(name)).unwrap();
        assert_eq!(node.display_name(false), hidden, "{name} without extension");
        assert_eq!(node.display_name(true), shown, "{name} with extension");
    }
}

#[test]
fn hidden_system_and_placeholder_attributes_are_reflected() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["plain.md", "hidden.md", "system.md", "cloud.md"] {
        touch(&dir.path().join(name), "x");
    }
    RealFs
        .set_attributes(
            &dir.path().join("hidden.md"),
            Attributes(Attributes::HIDDEN),
        )
        .unwrap();
    RealFs
        .set_attributes(
            &dir.path().join("system.md"),
            Attributes(Attributes::SYSTEM),
        )
        .unwrap();
    RealFs
        .set_attributes(
            &dir.path().join("cloud.md"),
            Attributes(Attributes::OFFLINE),
        )
        .unwrap();
    let ws = Workspace::open(real(), dir.path()).unwrap();
    let flags = |name: &str| {
        let node = ws.node(&dir.path().join(name)).unwrap();
        (node.is_hidden(), node.placeholder)
    };
    assert_eq!(flags("plain.md"), (false, false));
    assert_eq!(flags("hidden.md"), (true, false));
    assert_eq!(flags("system.md"), (true, false));
    assert_eq!(flags("cloud.md"), (false, true));
}

#[test]
fn folders_load_lazily_and_only_once() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a").join("b").join("deep.md"), "deep");
    touch(&dir.path().join("a").join("top.md"), "top");
    let a = dir.path().join("a");
    let b = a.join("b");
    let mut ws = Workspace::open(real(), dir.path()).unwrap();
    assert!(ws.node(&a).unwrap().children.is_none());
    assert!(ws.visible_children(&a, &ViewOptions::default()).is_empty());

    ws.load_children(&a).unwrap();
    assert_eq!(child_names(ws.node(&a).unwrap()), ["b", "top.md"]);
    assert!(ws.node(&b).unwrap().children.is_none());

    // A second call keeps the loaded children: the new file stays unseen until a rescan.
    touch(&a.join("late.md"), "late");
    ws.load_children(&a).unwrap();
    assert_eq!(child_names(ws.node(&a).unwrap()), ["b", "top.md"]);

    ws.load_children(&b).unwrap();
    assert_eq!(child_names(ws.node(&b).unwrap()), ["deep.md"]);
    // Folder names compare case-insensitively like NTFS.
    let shouting = PathBuf::from(dir.path().to_string_lossy().to_uppercase())
        .join("A")
        .join("B");
    assert_eq!(child_names(ws.node(&shouting).unwrap()), ["deep.md"]);
}

#[test]
fn load_children_rejects_foreign_missing_and_non_folder_paths() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    touch(&dir.path().join("file.md"), "x");
    let mut ws = Workspace::open(real(), dir.path()).unwrap();
    let outside = ws.load_children(other.path()).unwrap_err();
    assert_eq!(outside, FsError::NotFound(other.path().to_path_buf()));
    let missing = dir.path().join("nope");
    assert_eq!(
        ws.load_children(&missing).unwrap_err(),
        FsError::NotFound(missing.clone())
    );
    assert!(ws.load_children(&dir.path().join("file.md")).is_err());
    assert!(ws.node(other.path()).is_none());
    assert!(ws.node(&missing).is_none());
    assert!(ws.node(&dir.path().join("file.md").join("below")).is_none());
}

#[test]
fn load_all_expands_every_folder() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a").join("b").join("c").join("x.md"), "x");
    touch(&dir.path().join("a").join("y.md"), "y");
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    let mut ws = Workspace::open(real(), dir.path()).unwrap();
    ws.load_all().unwrap();
    for folder in ["a", "a\\b", "a\\b\\c", "empty"] {
        assert!(
            ws.node(&dir.path().join(folder))
                .unwrap()
                .children
                .is_some(),
            "{folder} is loaded"
        );
    }
    assert_eq!(
        child_names(ws.node(&dir.path().join("a\\b\\c")).unwrap()),
        ["x.md"]
    );
    // Loading again over an already loaded tree is harmless.
    ws.load_all().unwrap();
}

#[test]
fn load_all_does_not_follow_junctions() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("real").join("x.md"), "x");
    let link = dir.path().join("loop");
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(dir.path())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    assert!(status.status.success(), "mklink /J failed: {status:?}");

    let mut ws = Workspace::open(real(), dir.path()).unwrap();
    ws.load_all().unwrap();
    let node = ws.node(&link).unwrap();
    assert!(node.is_dir());
    assert!(node.attrs.any(Attributes::REPARSE_POINT));
    assert!(node.children.is_none(), "the junction cycle is not entered");
    assert!(
        ws.node(&dir.path().join("real"))
            .unwrap()
            .children
            .is_some()
    );
    // Remove the junction itself first so cleanup can never walk through it.
    std::fs::remove_dir(&link).unwrap();
}

#[test]
fn listing_errors_surface() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("sub").join("x.md"), "x");
    let missing = dir.path().join("missing");
    assert_eq!(
        Workspace::open(real(), &missing).unwrap_err(),
        FsError::NotFound(missing)
    );

    let fault = Arc::new(FaultFs::new());
    fault.fail(FsOp::ListDir, FsFailure::AccessDenied, 1);
    let denied = Workspace::open(fault.clone(), dir.path()).unwrap_err();
    assert_eq!(denied, FsError::AccessDenied(dir.path().to_path_buf()));

    let mut ws = Workspace::open(fault.clone(), dir.path()).unwrap();
    let sub = dir.path().join("sub");
    fault.fail_for_path(FsOp::ListDir, &sub, FsFailure::SharingViolation, 2);
    assert_eq!(
        ws.load_children(&sub).unwrap_err(),
        FsError::SharingViolation(sub.clone())
    );
    assert_eq!(
        ws.load_all().unwrap_err(),
        FsError::SharingViolation(sub.clone())
    );
    // The scripted failures are used up: the folder loads now.
    ws.load_all().unwrap();
    assert_eq!(child_names(ws.node(&sub).unwrap()), ["x.md"]);
}

fn sorted_tree() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    for folder in ["sub", "Zeta"] {
        std::fs::create_dir(dir.path().join(folder)).unwrap();
    }
    for file in ["file10.md", "file2.md", "file1.md"] {
        touch(&dir.path().join(file), "x");
    }
    let ws = Workspace::open(real(), dir.path()).unwrap();
    (dir, ws)
}

#[test]
fn names_sort_naturally_with_folders_first() {
    let (dir, ws) = sorted_tree();
    let options = ViewOptions::default();
    assert_eq!(
        listing(&ws, dir.path(), &options),
        ["sub", "Zeta", "file1.md", "file2.md", "file10.md"]
    );
    let descending = ViewOptions {
        descending: true,
        ..options
    };
    assert_eq!(
        listing(&ws, dir.path(), &descending),
        ["Zeta", "sub", "file10.md", "file2.md", "file1.md"]
    );
    let mixed = ViewOptions {
        folders_first: false,
        ..options
    };
    assert_eq!(
        listing(&ws, dir.path(), &mixed),
        ["file1.md", "file2.md", "file10.md", "sub", "Zeta"]
    );
}

#[test]
fn modified_and_created_sorts_use_the_timestamps_and_break_ties_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let base = 1_000_000_000;
    // (name, created, modified)
    let files = [
        ("a.md", base + 2000, base + 3000),
        ("b.md", base + 1000, base + 1000),
        ("c.md", base + 3000, base + 2000),
        ("d.md", base + 4000, base + 5000),
        ("e.md", base + 4000, base + 5000),
    ];
    for (name, created, modified) in files {
        let path = dir.path().join(name);
        touch(&path, "x");
        set_times(&path, created, modified);
    }
    let ws = Workspace::open(real(), dir.path()).unwrap();
    let by = |sort, descending| {
        listing(
            &ws,
            dir.path(),
            &ViewOptions {
                sort,
                descending,
                ..ViewOptions::default()
            },
        )
    };
    assert_eq!(
        by(SortKey::Modified, false),
        ["b.md", "c.md", "a.md", "d.md", "e.md"]
    );
    assert_eq!(
        by(SortKey::Modified, true),
        ["d.md", "e.md", "a.md", "c.md", "b.md"]
    );
    assert_eq!(
        by(SortKey::Created, false),
        ["b.md", "a.md", "c.md", "d.md", "e.md"]
    );
    assert_eq!(
        by(SortKey::Created, true),
        ["d.md", "e.md", "c.md", "a.md", "b.md"]
    );
}

#[test]
fn kind_sort_orders_by_extension_then_name() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["b.txt", "a.md", "c.md", "z.png"] {
        touch(&dir.path().join(name), "x");
    }
    let ws = Workspace::open(real(), dir.path()).unwrap();
    let kind = ViewOptions {
        sort: SortKey::Kind,
        ..ViewOptions::default()
    };
    assert_eq!(
        listing(&ws, dir.path(), &kind),
        ["a.md", "c.md", "z.png", "b.txt"]
    );
    let descending = ViewOptions {
        descending: true,
        ..kind
    };
    assert_eq!(
        listing(&ws, dir.path(), &descending),
        ["b.txt", "z.png", "a.md", "c.md"]
    );
}

#[test]
fn filters_hide_hidden_images_and_non_markdown_files() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "a.md",
        "pic.png",
        "notes.txt",
        "data.bin",
        "hidden.md",
        "sys.md",
    ] {
        touch(&dir.path().join(name), "x");
    }
    std::fs::create_dir(dir.path().join("folder")).unwrap();
    RealFs
        .set_attributes(
            &dir.path().join("hidden.md"),
            Attributes(Attributes::HIDDEN),
        )
        .unwrap();
    RealFs
        .set_attributes(&dir.path().join("sys.md"), Attributes(Attributes::SYSTEM))
        .unwrap();
    let ws = Workspace::open(real(), dir.path()).unwrap();
    let with = |options: ViewOptions| listing(&ws, dir.path(), &options);
    let base = ViewOptions::default();

    assert_eq!(
        with(base),
        ["folder", "a.md", "data.bin", "notes.txt", "pic.png"]
    );
    assert_eq!(
        with(ViewOptions {
            show_hidden: true,
            ..base
        }),
        [
            "folder",
            "a.md",
            "data.bin",
            "hidden.md",
            "notes.txt",
            "pic.png",
            "sys.md"
        ]
    );
    assert_eq!(
        with(ViewOptions {
            hide_images: true,
            ..base
        }),
        ["folder", "a.md", "data.bin", "notes.txt"]
    );
    assert_eq!(
        with(ViewOptions {
            hide_non_markdown: true,
            ..base
        }),
        ["folder", "a.md"]
    );
    assert_eq!(
        with(ViewOptions {
            hide_images: true,
            hide_non_markdown: true,
            show_hidden: true,
            ..base
        }),
        ["folder", "a.md", "hidden.md", "sys.md"]
    );
}

#[test]
fn logical_comparison_follows_explorer() {
    assert_eq!(compare_logical("file2", "file10"), Ordering::Less);
    assert_eq!(compare_logical("file10", "file2"), Ordering::Greater);
    assert_eq!(compare_logical("file2", "file2"), Ordering::Equal);
    assert_eq!(compare_logical("alpha", "beta"), Ordering::Less);
    assert_eq!(compare_logical("Alpha", "beta"), Ordering::Less);
    assert_eq!(compare_logical("alpha", "Beta"), Ordering::Less);
    assert_eq!(compare_logical("", "a"), Ordering::Less);
}

#[test]
fn turkish_dotted_and_dotless_i_stay_distinct_and_antisymmetric() {
    let letters = ["i", "I", "İ", "ı"];
    for left in letters {
        for right in letters {
            let forward = compare_logical(left, right);
            let backward = compare_logical(right, left);
            assert_eq!(forward, backward.reverse(), "{left} vs {right}");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    for name in ["Izmir.md", "İzmir.md", "ısparta.md", "isparta.md"] {
        touch(&dir.path().join(name), "x");
    }
    let ws = Workspace::open(real(), dir.path()).unwrap();
    let shown = listing(&ws, dir.path(), &ViewOptions::default());
    let mut present = shown.clone();
    present.sort();
    assert_eq!(
        present,
        ["Izmir.md", "isparta.md", "İzmir.md", "ısparta.md"]
    );
    let mut expected = shown.clone();
    expected.sort_by(|a, b| compare_logical(a, b));
    assert_eq!(shown, expected, "the sidebar order is the logical order");
}

fn watched_tree() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("sub").join("inner.md"), "inner");
    touch(&dir.path().join("cold").join("frozen.md"), "frozen");
    touch(&dir.path().join("keep.md"), "keep");
    let mut ws = Workspace::open(real(), dir.path()).unwrap();
    ws.load_children(&dir.path().join("sub")).unwrap();
    (dir, ws)
}

#[test]
fn created_and_modified_events_insert_and_refresh_nodes() {
    let (dir, mut ws) = watched_tree();
    let new = dir.path().join("new.md");
    touch(&new, "1234");
    assert!(ws.apply(&event(&new, FsEventKind::Created)).unwrap());
    assert_eq!(ws.node(&new).unwrap().size, 4);
    assert!(
        !ws.apply(&event(&new, FsEventKind::Created)).unwrap(),
        "nothing changed the second time"
    );

    touch(&new, "1234567890");
    assert!(ws.apply(&event(&new, FsEventKind::Modified)).unwrap());
    assert_eq!(ws.node(&new).unwrap().size, 10);
    assert!(!ws.apply(&event(&new, FsEventKind::Modified)).unwrap());

    // A file inside a loaded subfolder.
    let inner = dir.path().join("sub").join("more.md");
    touch(&inner, "m");
    assert!(ws.apply(&event(&inner, FsEventKind::Created)).unwrap());
    assert_eq!(
        child_names(ws.node(&dir.path().join("sub")).unwrap()),
        ["inner.md", "more.md"]
    );
}

#[test]
fn modified_events_adopt_the_case_reported_by_the_disk() {
    let (dir, mut ws) = watched_tree();
    std::fs::rename(dir.path().join("keep.md"), dir.path().join("Keep.md")).unwrap();
    assert!(
        ws.apply(&event(&dir.path().join("Keep.md"), FsEventKind::Modified))
            .unwrap()
    );
    assert!(
        ws.tree()
            .children
            .as_ref()
            .unwrap()
            .iter()
            .any(|n| n.name == "Keep.md")
    );
    assert!(
        !ws.tree()
            .children
            .as_ref()
            .unwrap()
            .iter()
            .any(|n| n.name == "keep.md")
    );
}

#[test]
fn events_outside_the_loaded_tree_are_ignored() {
    let (dir, mut ws) = watched_tree();
    let other = tempfile::tempdir().unwrap();
    touch(&other.path().join("x.md"), "x");
    assert!(
        !ws.apply(&event(&other.path().join("x.md"), FsEventKind::Created))
            .unwrap()
    );
    assert!(
        !ws.apply(&event(&other.path().join("x.md"), FsEventKind::Removed))
            .unwrap()
    );
    assert!(!ws.apply(&event(dir.path(), FsEventKind::Created)).unwrap());
    assert!(!ws.apply(&event(dir.path(), FsEventKind::Removed)).unwrap());

    // Below an unloaded folder nothing is tracked; the folder loads fresh later.
    let cold = dir.path().join("cold");
    let late = cold.join("late.md");
    touch(&late, "late");
    assert!(!ws.apply(&event(&late, FsEventKind::Created)).unwrap());
    assert!(ws.node(&late).is_none());
    // A path whose parent is not even known.
    let orphan = dir.path().join("nowhere").join("x.md");
    touch(&orphan, "x");
    assert!(
        ws.apply(&event(&orphan, FsEventKind::Created))
            .is_ok_and(|changed| !changed)
    );
    ws.load_children(&cold).unwrap();
    assert_eq!(
        child_names(ws.node(&cold).unwrap()),
        ["frozen.md", "late.md"]
    );
}

#[test]
fn removed_events_drop_nodes_once() {
    let (dir, mut ws) = watched_tree();
    let keep = dir.path().join("keep.md");
    std::fs::remove_file(&keep).unwrap();
    assert!(ws.apply(&event(&keep, FsEventKind::Removed)).unwrap());
    assert!(ws.node(&keep).is_none());
    assert!(!ws.apply(&event(&keep, FsEventKind::Removed)).unwrap());

    let inner = dir.path().join("sub").join("inner.md");
    std::fs::remove_file(&inner).unwrap();
    assert!(ws.apply(&event(&inner, FsEventKind::Removed)).unwrap());
    assert!(
        ws.node(&dir.path().join("sub"))
            .unwrap()
            .children
            .as_ref()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_file_that_vanished_before_the_event_is_dropped_again() {
    let (dir, mut ws) = watched_tree();
    let ghost = dir.path().join("ghost.md");
    touch(&ghost, "boo");
    assert!(ws.apply(&event(&ghost, FsEventKind::Created)).unwrap());
    std::fs::remove_file(&ghost).unwrap();
    assert!(ws.apply(&event(&ghost, FsEventKind::Modified)).unwrap());
    assert!(ws.node(&ghost).is_none());
    assert!(!ws.apply(&event(&ghost, FsEventKind::Modified)).unwrap());
}

#[test]
fn a_failing_metadata_call_is_treated_as_vanished() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a.md"), "a");
    let fault = Arc::new(FaultFs::new());
    let mut ws = Workspace::open(fault.clone(), dir.path()).unwrap();
    let a = dir.path().join("a.md");
    fault.fail_for_path(FsOp::Metadata, &a, FsFailure::AccessDenied, 1);
    assert!(ws.apply(&event(&a, FsEventKind::Modified)).unwrap());
    assert!(ws.node(&a).is_none());
}

#[test]
fn renames_move_nodes_and_keep_loaded_subtrees() {
    let (dir, mut ws) = watched_tree();
    let sub = dir.path().join("sub");
    let moved = dir.path().join("moved");
    std::fs::rename(&sub, &moved).unwrap();
    let renamed = event(&moved, FsEventKind::Renamed { from: sub.clone() });
    assert!(ws.apply(&renamed).unwrap());
    assert!(ws.node(&sub).is_none());
    assert_eq!(
        child_names(ws.node(&moved).unwrap()),
        ["inner.md"],
        "subtree carried over"
    );

    // A file renamed within the root.
    let keep = dir.path().join("keep.md");
    let kept = dir.path().join("kept.md");
    std::fs::rename(&keep, &kept).unwrap();
    assert!(
        ws.apply(&event(&kept, FsEventKind::Renamed { from: keep.clone() }))
            .unwrap()
    );
    assert!(ws.node(&keep).is_none());
    assert_eq!(ws.node(&kept).unwrap().size, 4);

    // A case-only rename.
    let upper = dir.path().join("KEPT.md");
    std::fs::rename(&kept, &upper).unwrap();
    assert!(
        ws.apply(&event(&upper, FsEventKind::Renamed { from: kept.clone() }))
            .unwrap()
    );
    assert!(
        ws.tree()
            .children
            .as_ref()
            .unwrap()
            .iter()
            .any(|n| n.name == "KEPT.md")
    );
    assert!(
        !ws.tree()
            .children
            .as_ref()
            .unwrap()
            .iter()
            .any(|n| n.name == "kept.md")
    );
}

#[test]
fn a_rename_from_an_unknown_place_acts_like_a_create_or_a_remove() {
    let (dir, mut ws) = watched_tree();
    let other = tempfile::tempdir().unwrap();
    touch(&other.path().join("in.md"), "in");
    let arrived = dir.path().join("in.md");
    std::fs::rename(other.path().join("in.md"), &arrived).unwrap();
    let from_outside = event(
        &arrived,
        FsEventKind::Renamed {
            from: other.path().join("in.md"),
        },
    );
    assert!(ws.apply(&from_outside).unwrap());
    assert_eq!(ws.node(&arrived).unwrap().size, 2);

    // Moved out of the tree: the target path is outside, the source is dropped.
    let keep = dir.path().join("keep.md");
    std::fs::rename(&keep, other.path().join("keep.md")).unwrap();
    let to_outside = event(
        &other.path().join("keep.md"),
        FsEventKind::Renamed { from: keep.clone() },
    );
    assert!(ws.apply(&to_outside).unwrap());
    assert!(ws.node(&keep).is_none());

    // Neither side is known: nothing changes.
    let unrelated = event(
        &other.path().join("b.md"),
        FsEventKind::Renamed {
            from: other.path().join("a.md"),
        },
    );
    assert!(!ws.apply(&unrelated).unwrap());
}

#[test]
fn rescan_rereads_loaded_folders_and_keeps_unloaded_ones_cold() {
    let (dir, mut ws) = watched_tree();
    touch(&dir.path().join("added.md"), "a");
    touch(&dir.path().join("sub").join("added-inner.md"), "ai");
    std::fs::remove_file(dir.path().join("keep.md")).unwrap();
    std::fs::remove_file(dir.path().join("sub").join("inner.md")).unwrap();
    touch(&dir.path().join("cold").join("unseen.md"), "u");

    assert!(ws.apply(&event(dir.path(), FsEventKind::Rescan)).unwrap());
    assert_eq!(child_names(ws.tree()), ["added.md", "cold", "sub"]);
    assert_eq!(
        child_names(ws.node(&dir.path().join("sub")).unwrap()),
        ["added-inner.md"]
    );
    assert!(
        ws.node(&dir.path().join("cold"))
            .unwrap()
            .children
            .is_none(),
        "an unloaded folder stays unloaded"
    );
}

#[test]
fn rescan_reports_listing_failures() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a.md"), "a");
    let fault = Arc::new(FaultFs::new());
    let mut ws = Workspace::open(fault.clone(), dir.path()).unwrap();
    fault.fail(FsOp::ListDir, FsFailure::DiskFull, 1);
    let error = ws
        .apply(&event(dir.path(), FsEventKind::Rescan))
        .unwrap_err();
    assert_eq!(error, FsError::DiskFull(dir.path().to_path_buf()));
}

#[test]
fn rescan_reports_a_loaded_subfolder_that_cannot_be_listed() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("sub").join("x.md"), "x");
    let fault = Arc::new(FaultFs::new());
    let mut ws = Workspace::open(fault.clone(), dir.path()).unwrap();
    let sub = dir.path().join("sub");
    ws.load_children(&sub).unwrap();
    // The folder itself lists fine; its loaded subfolder does not.
    fault.fail_for_path(FsOp::ListDir, &sub, FsFailure::AccessDenied, 1);
    let error = ws
        .apply(&event(dir.path(), FsEventKind::Rescan))
        .unwrap_err();
    assert_eq!(error, FsError::AccessDenied(sub));
}

#[test]
fn events_below_folders_that_are_not_loaded_are_ignored() {
    let (dir, mut ws) = watched_tree();
    // `cold` was never expanded: nothing below it is tracked, however deep.
    let deep = dir.path().join("cold").join("deeper").join("x.md");
    touch(&deep, "x");
    assert!(!ws.apply(&event(&deep, FsEventKind::Created)).unwrap());
    // Neither is a removal there, nor one below a folder the tree does not know at all.
    let frozen = dir.path().join("cold").join("frozen.md");
    assert!(!ws.apply(&event(&frozen, FsEventKind::Removed)).unwrap());
    let ghost = dir.path().join("nowhere").join("x.md");
    assert!(!ws.apply(&event(&ghost, FsEventKind::Removed)).unwrap());

    // Nothing was disturbed: the folder loads fresh, with everything on disk.
    let cold = dir.path().join("cold");
    ws.load_children(&cold).unwrap();
    assert_eq!(
        child_names(ws.node(&cold).unwrap()),
        ["deeper", "frozen.md"]
    );
}

#[test]
fn folders_come_first_whichever_order_the_disk_lists_them_in() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a-folder")).unwrap();
    touch(&dir.path().join("b-file.md"), "x");
    std::fs::create_dir(dir.path().join("c-folder")).unwrap();
    touch(&dir.path().join("d-file.md"), "x");
    let ws = Workspace::open(real(), dir.path()).unwrap();

    let ascending = ViewOptions::default();
    assert_eq!(
        listing(&ws, dir.path(), &ascending),
        ["a-folder", "c-folder", "b-file.md", "d-file.md"]
    );
    let descending = ViewOptions {
        descending: true,
        ..ascending
    };
    assert_eq!(
        listing(&ws, dir.path(), &descending),
        ["c-folder", "a-folder", "d-file.md", "b-file.md"]
    );
}

#[test]
fn the_documents_folder_exists() {
    let documents = known_documents_dir().unwrap();
    assert!(documents.is_absolute());
    assert!(documents.is_dir(), "{documents:?}");
}

#[test]
fn the_first_run_folder_gets_a_welcome_file_that_is_never_overwritten() {
    let documents = tempfile::tempdir().unwrap();
    let folder = ensure_default_folder(&RealFs, documents.path(), "# Welcome\n").unwrap();
    assert_eq!(folder, documents.path().join(DEFAULT_FOLDER_NAME));
    let welcome = folder.join(WELCOME_FILE_NAME);
    assert_eq!(std::fs::read_to_string(&welcome).unwrap(), "# Welcome\n");

    std::fs::write(&welcome, "my own notes").unwrap();
    let again = ensure_default_folder(&RealFs, documents.path(), "# Welcome\n").unwrap();
    assert_eq!(again, folder);
    assert_eq!(std::fs::read_to_string(&welcome).unwrap(), "my own notes");
}

#[test]
fn first_run_failures_are_reported() {
    let documents = tempfile::tempdir().unwrap();
    let fault = FaultFs::new();
    fault.fail(FsOp::CreateDirAll, FsFailure::AccessDenied, 1);
    let denied = ensure_default_folder(&fault, documents.path(), "x").unwrap_err();
    assert_eq!(
        denied,
        FsError::AccessDenied(documents.path().join(DEFAULT_FOLDER_NAME))
    );
    assert!(!documents.path().join(DEFAULT_FOLDER_NAME).exists());

    fault.fail(FsOp::WriteNew, FsFailure::DiskFull, 1);
    let full = ensure_default_folder(&fault, documents.path(), "x").unwrap_err();
    assert!(matches!(full, FsError::DiskFull(_)));
}

#[test]
fn untitled_names_fill_the_first_gap() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(UNTITLED_BASE_NAME, "Untitled");
    assert_eq!(
        untitled_name(&RealFs, dir.path(), "md").unwrap(),
        "Untitled.md"
    );
    touch(&dir.path().join("Untitled.md"), "");
    assert_eq!(
        untitled_name(&RealFs, dir.path(), "md").unwrap(),
        "Untitled 2.md"
    );
    touch(&dir.path().join("Untitled 3.md"), "");
    assert_eq!(
        untitled_name(&RealFs, dir.path(), "md").unwrap(),
        "Untitled 2.md"
    );
    touch(&dir.path().join("Untitled 2.md"), "");
    assert_eq!(
        untitled_name(&RealFs, dir.path(), "md").unwrap(),
        "Untitled 4.md"
    );
    assert_eq!(
        untitled_name(&RealFs, dir.path(), "txt").unwrap(),
        "Untitled.txt"
    );
    assert_eq!(untitled_name(&RealFs, dir.path(), "").unwrap(), "Untitled");

    let fault = FaultFs::new();
    fault.fail(FsOp::Metadata, FsFailure::AccessDenied, 1);
    assert!(matches!(
        untitled_name(&fault, dir.path(), "md"),
        Err(FsError::AccessDenied(_))
    ));
}
