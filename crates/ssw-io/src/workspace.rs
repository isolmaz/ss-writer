//! The workspace tree (spec §5): lazily loaded folders, incremental watcher updates, Explorer-like
//! sorting (`StrCmpLogicalW`) and the sidebar filters.

use std::cmp::Ordering;
use std::ffi::c_void;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath, StrCmpLogicalW,
};
use windows::core::PCWSTR;

use crate::fileops::{NameStyle, free_name};
use crate::fs::{Attributes, DirEntry, FileMeta, FileSystem, FileTime, FsError, wide, win};
use crate::watch::{FsEvent, FsEventKind};

/// Folder of the default workspace created on first run, below `Documents`.
pub const DEFAULT_FOLDER_NAME: &str = "SS Writer";
/// Name of the welcome document created on first run.
pub const WELCOME_FILE_NAME: &str = "Welcome.md";
/// Base name of new, unsaved documents.
pub const UNTITLED_BASE_NAME: &str = "Untitled";

/// Kind of tree entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A file.
    File,
    /// A folder.
    Dir,
}

/// What the editor does with a file, decided by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
    /// `.md`, `.markdown`, `.mdx`: Markdown mode.
    Markdown,
    /// Raster and vector images: opened in the image viewer tab.
    Image,
    /// Other text formats: plain mode.
    Text,
    /// Everything else: dimmed, opened by the default app.
    Other,
}

/// Classifies an extension (without the dot, any case).
#[must_use]
pub fn classify_extension(ext: &str) -> FileClass {
    const MARKDOWN: [&str; 3] = ["md", "markdown", "mdx"];
    const IMAGE: [&str; 11] = [
        "png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "heic", "ico", "svg",
    ];
    const TEXT: [&str; 36] = [
        "txt", "text", "log", "csv", "tsv", "json", "jsonc", "yaml", "yml", "ini", "toml", "xml",
        "html", "htm", "css", "js", "ts", "tsx", "jsx", "py", "rs", "go", "java", "c", "h", "cpp",
        "cs", "sh", "bat", "ps1", "sql", "tex", "rst", "mmd", "cfg", "conf",
    ];
    let lower = ext.to_lowercase();
    let lower = lower.as_str();
    if MARKDOWN.contains(&lower) {
        FileClass::Markdown
    } else if IMAGE.contains(&lower) {
        FileClass::Image
    } else if TEXT.contains(&lower) {
        FileClass::Text
    } else {
        FileClass::Other
    }
}

/// One entry of the workspace tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// File or folder name.
    pub name: String,
    /// File or folder.
    pub kind: NodeKind,
    /// Lowercase extension without the dot (empty for folders and extensionless files).
    pub ext: String,
    /// Size in bytes (0 for folders).
    pub size: u64,
    /// Creation time.
    pub created: FileTime,
    /// Last write time.
    pub modified: FileTime,
    /// Attribute bits.
    pub attrs: Attributes,
    /// OneDrive-style placeholder: never hydrated by indexing or search.
    pub placeholder: bool,
    /// `None` until the folder is loaded (always `None` for files).
    pub children: Option<Vec<Node>>,
}

impl Node {
    fn new(name: String, meta: &FileMeta) -> Self {
        let kind = if meta.is_dir() {
            NodeKind::Dir
        } else {
            NodeKind::File
        };
        let ext = if kind == NodeKind::Dir {
            String::new()
        } else {
            Path::new(&name)
                .extension()
                .map(|ext| ext.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        };
        Self {
            name,
            kind,
            ext,
            size: meta.size,
            created: meta.created,
            modified: meta.modified,
            attrs: meta.attrs,
            placeholder: meta.is_placeholder(),
            children: None,
        }
    }

    fn from_entry(entry: &DirEntry) -> Self {
        Self::new(entry.name.clone(), &entry.meta)
    }

    fn update_meta(&mut self, meta: &FileMeta) {
        self.size = meta.size;
        self.created = meta.created;
        self.modified = meta.modified;
        self.attrs = meta.attrs;
        self.placeholder = meta.is_placeholder();
    }

    /// Whether this is a folder.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    /// File classification (folders are [`FileClass::Other`]).
    #[must_use]
    pub fn class(&self) -> FileClass {
        if self.is_dir() {
            FileClass::Other
        } else {
            classify_extension(&self.ext)
        }
    }

    /// Hidden or system item.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.attrs.is_hidden() || self.attrs.is_system()
    }

    /// The name shown in the tree: without the extension when `show_extensions` is off.
    #[must_use]
    pub fn display_name(&self, show_extensions: bool) -> &str {
        if show_extensions || self.is_dir() {
            return &self.name;
        }
        match self.name.rfind('.') {
            Some(index) if index > 0 => &self.name[..index],
            _ => &self.name,
        }
    }
}

/// Sort key of the tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    /// Natural name order (`file2` < `file10`).
    #[default]
    Name,
    /// Last write time.
    Modified,
    /// Creation time.
    Created,
    /// Extension, then name.
    Kind,
}

/// Sidebar sorting and filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewOptions {
    /// Sort key.
    pub sort: SortKey,
    /// Reverse the order (folders stay first when `folders_first`).
    pub descending: bool,
    /// List folders before files.
    pub folders_first: bool,
    /// Show hidden and system items.
    pub show_hidden: bool,
    /// Hide image files.
    pub hide_images: bool,
    /// Hide every file that is not Markdown.
    pub hide_non_markdown: bool,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            sort: SortKey::Name,
            descending: false,
            folders_first: true,
            show_hidden: false,
            hide_images: false,
            hide_non_markdown: false,
        }
    }
}

/// Explorer-style natural comparison (`StrCmpLogicalW`): digit runs compare by value and
/// letters compare case-insensitively with the user's locale rules (Turkish İ/ı included).
#[must_use]
pub fn compare_logical(a: &str, b: &str) -> Ordering {
    let left = wide(Path::new(a));
    let right = wide(Path::new(b));
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call.
    let result = unsafe { StrCmpLogicalW(PCWSTR(left.as_ptr()), PCWSTR(right.as_ptr())) };
    result.cmp(&0)
}

fn compare_nodes(a: &Node, b: &Node, options: &ViewOptions) -> Ordering {
    if options.folders_first && a.is_dir() != b.is_dir() {
        return if a.is_dir() {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let by_name = compare_logical(&a.name, &b.name);
    let primary = match options.sort {
        SortKey::Name => by_name,
        SortKey::Modified => a.modified.cmp(&b.modified),
        SortKey::Created => a.created.cmp(&b.created),
        SortKey::Kind => compare_logical(&a.ext, &b.ext),
    };
    let primary = if options.descending {
        primary.reverse()
    } else {
        primary
    };
    primary.then(by_name)
}

fn visible(node: &Node, options: &ViewOptions) -> bool {
    if node.is_hidden() && !options.show_hidden {
        return false;
    }
    if node.is_dir() {
        return true;
    }
    let class = node.class();
    if options.hide_images && class == FileClass::Image {
        return false;
    }
    !(options.hide_non_markdown && class != FileClass::Markdown)
}

/// Splits `path` below `root` into components, comparing case-insensitively.
fn relative_components(root: &Path, path: &Path) -> Option<Vec<String>> {
    let lower = |component: Component<'_>| component.as_os_str().to_string_lossy().to_lowercase();
    let mut path_iter = path.components();
    for expected in root.components().map(lower) {
        if path_iter.next().map(lower) != Some(expected) {
            return None;
        }
    }
    Some(
        path_iter
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect(),
    )
}

fn find_child<'a>(node: &'a mut Node, name: &str) -> Option<&'a mut Node> {
    let wanted = name.to_lowercase();
    node.children
        .as_mut()?
        .iter_mut()
        .find(|child| child.name.to_lowercase() == wanted)
}

fn find_node<'a>(root: &'a mut Node, parts: &[String]) -> Option<&'a mut Node> {
    let mut node = root;
    for part in parts {
        node = find_child(node, part)?;
    }
    Some(node)
}

/// Lists `path` into `node`, keeping the loaded subtrees of folders that are still there.
fn reload_node(fs: &dyn FileSystem, node: &mut Node, path: &Path) -> Result<(), FsError> {
    let mut fresh: Vec<Node> = fs.list_dir(path)?.iter().map(Node::from_entry).collect();
    if let Some(mut old) = node.children.take() {
        for new in &mut fresh {
            if let Some(previous) = old.iter_mut().find(|candidate| candidate.name == new.name) {
                new.children = previous.children.take();
            }
        }
    }
    node.children = Some(fresh);
    Ok(())
}

fn rescan_node(fs: &dyn FileSystem, node: &mut Node, path: &Path) -> Result<(), FsError> {
    reload_node(fs, node, path)?;
    for child in node.children.iter_mut().flatten() {
        if child.is_dir() && child.children.is_some() {
            rescan_node(fs, child, &path.join(&child.name))?;
        }
    }
    Ok(())
}

fn load_all_node(fs: &dyn FileSystem, node: &mut Node, path: &Path) -> Result<(), FsError> {
    if node.children.is_none() {
        reload_node(fs, node, path)?;
    }
    for child in node.children.iter_mut().flatten() {
        if child.is_dir() && !child.attrs.any(Attributes::REPARSE_POINT) {
            load_all_node(fs, child, &path.join(&child.name))?;
        }
    }
    Ok(())
}

/// The lazily loaded tree of one workspace root.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    fs: Arc<dyn FileSystem>,
    tree: Node,
}

impl Workspace {
    /// Opens `root` and loads its direct children.
    ///
    /// # Errors
    /// [`FsError::NotFound`] when the folder does not exist.
    pub fn open(fs: Arc<dyn FileSystem>, root: &Path) -> Result<Self, FsError> {
        let meta = fs.metadata(root)?;
        let name = root.file_name().unwrap_or(root.as_os_str());
        let mut tree = Node::new(
            name.to_string_lossy().into_owned(),
            &FileMeta {
                attrs: meta.attrs.with(Attributes::DIRECTORY),
                ..meta
            },
        );
        reload_node(fs.as_ref(), &mut tree, root)?;
        Ok(Self {
            root: root.to_path_buf(),
            fs,
            tree,
        })
    }

    /// The root folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The root node.
    #[must_use]
    pub fn tree(&self) -> &Node {
        &self.tree
    }

    /// The node at `path` (absolute, inside the root).
    #[must_use]
    pub fn node(&self, path: &Path) -> Option<&Node> {
        let parts = relative_components(&self.root, path)?;
        let mut node = &self.tree;
        for part in parts {
            let wanted = part.to_lowercase();
            node = node
                .children
                .as_ref()?
                .iter()
                .find(|child| child.name.to_lowercase() == wanted)?;
        }
        Some(node)
    }

    /// Loads the children of the folder at `path` unless they are already loaded.
    ///
    /// # Errors
    /// [`FsError::NotFound`] when `path` is outside the root or not a known folder, plus any
    /// listing error.
    pub fn load_children(&mut self, path: &Path) -> Result<(), FsError> {
        let missing = FsError::NotFound(path.to_path_buf());
        let parts = relative_components(&self.root, path).ok_or(missing.clone())?;
        let node = find_node(&mut self.tree, &parts).ok_or(missing)?;
        if node.children.is_some() {
            return Ok(());
        }
        reload_node(self.fs.as_ref(), node, path)
    }

    /// Loads every folder below the root ("Expand all"); reparse points are not followed.
    ///
    /// # Errors
    /// The first listing error.
    pub fn load_all(&mut self) -> Result<(), FsError> {
        load_all_node(self.fs.as_ref(), &mut self.tree, &self.root)
    }

    /// The visible children of the folder at `path`, sorted. Unloaded folders give an empty list.
    #[must_use]
    pub fn visible_children(&self, path: &Path, options: &ViewOptions) -> Vec<&Node> {
        let Some(children) = self.node(path).and_then(|node| node.children.as_ref()) else {
            return Vec::new();
        };
        let mut shown: Vec<&Node> = children
            .iter()
            .filter(|node| visible(node, options))
            .collect();
        shown.sort_by(|a, b| compare_nodes(a, b, options));
        shown
    }

    /// Applies one watcher event to the loaded part of the tree. Returns whether the tree
    /// changed. Events below unloaded folders are ignored (they load fresh when expanded).
    ///
    /// # Errors
    /// Listing errors of a [`FsEventKind::Rescan`].
    pub fn apply(&mut self, event: &FsEvent) -> Result<bool, FsError> {
        match &event.kind {
            FsEventKind::Rescan => {
                rescan_node(self.fs.as_ref(), &mut self.tree, &self.root)?;
                Ok(true)
            }
            FsEventKind::Removed => Ok(self.remove(&event.path).is_some()),
            FsEventKind::Created | FsEventKind::Modified => Ok(self.upsert(&event.path, None)),
            FsEventKind::Renamed { from } => {
                let moved = self.remove(from);
                let removed = moved.is_some();
                let upserted = self.upsert(&event.path, moved);
                Ok(removed || upserted)
            }
        }
    }

    fn remove(&mut self, path: &Path) -> Option<Node> {
        let mut parts = relative_components(&self.root, path)?;
        let name = parts.pop()?;
        let parent = find_node(&mut self.tree, &parts)?;
        let wanted = name.to_lowercase();
        let children = parent.children.as_mut()?;
        let position = children
            .iter()
            .position(|child| child.name.to_lowercase() == wanted)?;
        Some(children.remove(position))
    }

    /// Inserts or refreshes the node at `path` from the disk. `carried` is a node moved from
    /// elsewhere whose loaded subtree should survive. Returns whether the tree changed.
    fn upsert(&mut self, path: &Path, carried: Option<Node>) -> bool {
        let Some(mut parts) = relative_components(&self.root, path) else {
            return false;
        };
        let Some(name) = parts.pop() else {
            return false;
        };
        let Ok(meta) = self.fs.metadata(path) else {
            // Gone again before we looked: drop a stale entry.
            return self.remove(path).is_some();
        };
        let Some(parent) = find_node(&mut self.tree, &parts) else {
            return false;
        };
        let Some(children) = parent.children.as_mut() else {
            return false;
        };
        let wanted = name.to_lowercase();
        if let Some(existing) = children
            .iter_mut()
            .find(|child| child.name.to_lowercase() == wanted)
        {
            let before = existing.clone();
            existing.name = name;
            existing.update_meta(&meta);
            return *existing != before;
        }
        let mut node = Node::new(name, &meta);
        if let Some(moved) = carried {
            node.children = moved.children;
        }
        children.push(node);
        true
    }
}

/// First free `Untitled.md`, `Untitled 2.md`, … in `dir`.
///
/// # Errors
/// Failures of the existence checks other than "not found".
pub fn untitled_name(fs: &dyn FileSystem, dir: &Path, ext: &str) -> Result<String, FsError> {
    free_name(fs, dir, UNTITLED_BASE_NAME, ext, NameStyle::Numbered)
}

/// The user's `Documents` folder.
///
/// # Errors
/// The shell error when the known folder cannot be resolved.
pub fn known_documents_dir() -> Result<PathBuf, FsError> {
    // SAFETY: `FOLDERID_Documents` is a valid GUID; no token means the current user.
    let pointer = win(
        unsafe { SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None) },
        Path::new("Documents"),
    )?;
    // SAFETY: on success the shell returns a NUL-terminated string that we own and free below.
    let text = String::from_utf16_lossy(unsafe { pointer.as_wide() });
    // SAFETY: memory returned by `SHGetKnownFolderPath` must be released with `CoTaskMemFree`.
    unsafe { CoTaskMemFree(Some(pointer.0.cast::<c_void>().cast_const())) };
    Ok(PathBuf::from(text))
}

/// First-run workspace: creates `<documents>\SS Writer\Welcome.md` with `welcome` as content
/// (an existing file is never overwritten) and returns the folder.
///
/// # Errors
/// Failures creating the folder or the file.
pub fn ensure_default_folder(
    fs: &dyn FileSystem,
    documents: &Path,
    welcome: &str,
) -> Result<PathBuf, FsError> {
    let folder = documents.join(DEFAULT_FOLDER_NAME);
    fs.create_dir_all(&folder)?;
    match fs.write_new(&folder.join(WELCOME_FILE_NAME), welcome.as_bytes()) {
        Ok(()) | Err(FsError::AlreadyExists(_)) => Ok(folder),
        Err(error) => Err(error),
    }
}
