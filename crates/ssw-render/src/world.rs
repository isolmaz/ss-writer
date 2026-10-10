//! The Typst [`World`]: in-memory main source, bundled fonts, optional font files, virtual
//! files, file access restricted to one allow-listed directory and no network.
//!
//! One instance is meant to be reused: Typst's `comemo` cache keeps everything that did not
//! change between two compilations, so recompiling a similar document costs milliseconds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use typst::diag::{FileError, FileResult, Severity};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

use crate::fonts::{FontError, FontSet};

/// Virtual directory that holds the vendored MiTeX command specification.
pub const MITEX_DIR: &str = "mitex";

const MITEX_FILES: [(&str, &str); 3] = [
    (
        "mitex/mod.typ",
        include_str!("../../../third_party/mitex-spec/mod.typ"),
    ),
    (
        "mitex/prelude.typ",
        include_str!("../../../third_party/mitex-spec/prelude.typ"),
    ),
    (
        "mitex/latex/standard.typ",
        include_str!("../../../third_party/mitex-spec/latex/standard.typ"),
    ),
];

/// A Typst compilation failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct CompileError {
    /// The error messages, joined with ` | `.
    pub message: String,
    /// Hints attached to the errors.
    pub hints: Vec<String>,
}

/// A virtual file name that Typst cannot represent (for example one that escapes the root).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid virtual file path {0:?}")]
pub struct InvalidPath(pub String);

/// Today's date as `(year, month, day)`.
pub type Date = (i32, u8, u8);

/// Typst world implementation. All methods take `&self`; compilations are serialized internally
/// because they share the main source.
pub struct TypstWorld {
    library: LazyHash<Library>,
    fonts: FontSet,
    main: FileId,
    main_source: Mutex<Source>,
    /// Canonical allow-listed directory; `None` = no disk access at all.
    root: Option<PathBuf>,
    virtual_files: Mutex<HashMap<FileId, Bytes>>,
    sources: Mutex<HashMap<FileId, Source>>,
    files: Mutex<HashMap<FileId, Bytes>>,
    today: Mutex<Option<Date>>,
    compile_lock: Mutex<()>,
}

impl std::fmt::Debug for TypstWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypstWorld")
            .field("root", &self.root)
            .field("fonts", &self.fonts)
            .finish()
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding one of these locks cannot leave the data inconsistent.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn project_id(path: &str) -> Result<FileId, InvalidPath> {
    let vpath = VirtualPath::new(path).map_err(|_| InvalidPath(path.to_owned()))?;
    Ok(RootedPath::new(VirtualRoot::Project, vpath).intern())
}

impl TypstWorld {
    /// A world with the bundled fonts. File reads (`image(...)`, `read(...)`, imports) are only
    /// allowed below `root` (the directory of the open document); with `None` nothing is read
    /// from disk.
    pub fn new(root: Option<&Path>) -> Self {
        let main_source = Source::detached(String::new());
        let main = main_source.id();
        let virtual_files = MITEX_FILES
            .iter()
            .filter_map(|(name, text)| {
                project_id(name)
                    .ok()
                    .map(|id| (id, Bytes::from_string(*text)))
            })
            .collect();
        Self {
            library: LazyHash::new(Library::default()),
            fonts: FontSet::bundled(),
            main,
            main_source: Mutex::new(main_source),
            root: root.and_then(|r| std::fs::canonicalize(r).ok()),
            virtual_files: Mutex::new(virtual_files),
            sources: Mutex::default(),
            files: Mutex::default(),
            today: Mutex::new(None),
            compile_lock: Mutex::new(()),
        }
    }

    /// The canonical allow-listed directory.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Fixes the value returned by Typst's `datetime.today()` (default: the system date, UTC).
    pub fn set_today(&self, date: Option<Date>) {
        *lock(&self.today) = date;
    }

    /// Adds the faces of a font file; they are parsed when first used. Returns the face count.
    pub fn add_font_file(&mut self, path: &Path) -> Result<usize, FontError> {
        self.fonts.add_file(path)
    }

    /// Registers an in-memory file at `path` (relative to the project root, e.g. `diagram-1.svg`).
    /// It shadows a file on disk with the same path.
    pub fn set_virtual_file(&self, path: &str, bytes: Vec<u8>) -> Result<(), InvalidPath> {
        let id = project_id(path)?;
        lock(&self.virtual_files).insert(id, Bytes::new(bytes));
        lock(&self.sources).remove(&id);
        Ok(())
    }

    /// Forgets files read from disk (call after the files may have changed).
    pub fn clear_file_cache(&self) {
        lock(&self.files).clear();
        lock(&self.sources).clear();
    }

    /// Whether `path` (relative to the project root) names a readable file inside the
    /// allow-listed directory, or a virtual file.
    pub fn file_available(&self, path: &str) -> bool {
        match project_id(path) {
            Ok(id) => lock(&self.virtual_files).contains_key(&id) || self.disk_path(id).is_ok(),
            Err(_) => false,
        }
    }

    /// Compiles `source` as the main file. Calls are serialized.
    pub fn compile(&self, source: &str) -> Result<PagedDocument, CompileError> {
        let _guard = lock(&self.compile_lock);
        lock(&self.main_source).replace(source);
        let warned = typst::compile::<PagedDocument>(self);
        warned.output.map_err(|diagnostics| {
            let errors = || diagnostics.iter().filter(|d| d.severity == Severity::Error);
            CompileError {
                message: errors()
                    .map(|d| d.message.to_string())
                    .collect::<Vec<_>>()
                    .join(" | "),
                hints: errors()
                    .flat_map(|d| d.hints.iter().map(|h| h.v.to_string()))
                    .collect(),
            }
        })
    }

    /// The real path of a project file, checked against the allow-list.
    fn disk_path(&self, id: FileId) -> FileResult<PathBuf> {
        let rooted = id.get();
        let (VirtualRoot::Project, Some(root)) = (rooted.root(), self.root.as_deref()) else {
            return Err(FileError::AccessDenied);
        };
        let path = rooted
            .vpath()
            .realize(root)
            .map_err(|_| FileError::AccessDenied)?;
        // Symbolic links and junctions may point out of the allow-listed directory.
        let real = std::fs::canonicalize(&path).map_err(|e| FileError::from_io(e, &path))?;
        if real.starts_with(root) {
            Ok(real)
        } else {
            Err(FileError::AccessDenied)
        }
    }

    fn read_bytes(&self, id: FileId) -> FileResult<Bytes> {
        if let Some(bytes) = lock(&self.virtual_files).get(&id) {
            return Ok(bytes.clone());
        }
        if let Some(bytes) = lock(&self.files).get(&id) {
            return Ok(bytes.clone());
        }
        let path = self.disk_path(id)?;
        let data = std::fs::read(&path).map_err(|e| FileError::from_io(e, &path))?;
        let bytes = Bytes::new(data);
        lock(&self.files).insert(id, bytes.clone());
        Ok(bytes)
    }
}

/// Proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // Month 1..=12 and day 1..=31 always fit; years are far inside i32.
    (year as i32, month as u8, day as u8)
}

fn system_today() -> Date {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0))
}

impl World for TypstWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main {
            return Ok(lock(&self.main_source).clone());
        }
        if let Some(source) = lock(&self.sources).get(&id) {
            return Ok(source.clone());
        }
        let bytes = self.read_bytes(id)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| FileError::InvalidUtf8)?;
        let source = Source::new(id, text.to_owned());
        lock(&self.sources).insert(id, source.clone());
        Ok(source)
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.main {
            return Ok(Bytes::from_string(
                lock(&self.main_source).text().to_owned(),
            ));
        }
        self.read_bytes(id)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        let (year, month, day) = lock(&self.today).unwrap_or_else(system_today);
        Datetime::from_ymd(year, month, day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ssw-render-world-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_main_file_is_served_from_the_current_source_text() {
        let world = TypstWorld::new(None);
        world.compile("hello world").expect("compiles");
        let bytes = World::file(&world, World::main(&world)).expect("main file");
        assert!(String::from_utf8_lossy(bytes.as_slice()).contains("hello world"));
    }

    fn first_text(doc: &PagedDocument) -> String {
        let mut out = String::new();
        for (_, item) in doc.pages()[0].frame.items() {
            if let typst::layout::FrameItem::Text(t) = item {
                out.push_str(&t.text);
            }
        }
        out
    }

    #[test]
    fn civil_dates() {
        let table = [
            (0, (1970, 1, 1)),
            (59, (1970, 3, 1)),
            (365, (1971, 1, 1)),
            (11_016, (2000, 2, 29)),
            (20_371, (2025, 10, 10)),
            (-1, (1969, 12, 31)),
        ];
        for (days, expected) in table {
            assert_eq!(civil_from_days(days), expected, "day {days}");
        }
        let (y, m, d) = system_today();
        assert!(y >= 2024 && (1..=12).contains(&m) && (1..=31).contains(&d));
    }

    #[test]
    fn compiles_main_source_and_reports_errors() {
        let world = TypstWorld::new(None);
        let doc = world.compile("Hello *world*").expect("compiles");
        assert_eq!(doc.pages().len(), 1);
        assert!(first_text(&doc).contains("Hello"));

        let err = world.compile("#nosuchfunction()").expect_err("fails");
        assert!(err.message.contains("unknown variable"), "{err}");
        let hinted = world.compile("#let x = 1\n#x.foo").expect_err("fails");
        assert!(!hinted.message.is_empty());
        // The same world keeps working after an error.
        assert!(world.compile("ok").is_ok());
        assert!(format!("{world:?}").contains("TypstWorld"));
    }

    #[test]
    fn today_is_injectable() {
        let world = TypstWorld::new(None);
        world.set_today(Some((2026, 10, 10)));
        let doc = world
            .compile("#datetime.today().display(\"[year]-[month]-[day]\")")
            .expect("compiles");
        assert!(first_text(&doc).contains("2026-10-10"));
        world.set_today(None);
        let doc = world.compile("#datetime.today().year()").expect("compiles");
        assert!(first_text(&doc).starts_with("20"));
    }

    #[test]
    fn virtual_files_shadow_and_validate() {
        let world = TypstWorld::new(None);
        world
            .set_virtual_file("data/note.txt", b"from memory".to_vec())
            .expect("valid path");
        assert!(world.file_available("data/note.txt"));
        assert!(!world.file_available("data/other.txt"));
        let doc = world.compile("#read(\"data/note.txt\")").expect("compiles");
        assert!(first_text(&doc).contains("from memory"));
        assert!(world.set_virtual_file("../escape.txt", Vec::new()).is_err());
        assert!(!world.file_available("../escape.txt"));
        let err = InvalidPath("x".into());
        assert!(err.to_string().contains("\"x\""));
    }

    #[test]
    fn disk_access_is_limited_to_the_root() {
        let outer = temp_dir("outer");
        let root = outer.join("doc");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("inside.txt"), "inside").expect("write");
        std::fs::write(outer.join("secret.txt"), "secret").expect("write");

        let world = TypstWorld::new(Some(&root));
        assert!(world.root().is_some());
        assert!(world.file_available("inside.txt"));
        assert!(!world.file_available("missing.txt"));
        assert!(!world.file_available("../secret.txt"));
        let doc = world
            .compile("#read(\"inside.txt\")")
            .expect("reads inside");
        assert!(first_text(&doc).contains("inside"));
        // Cached bytes survive a second read; clearing the cache re-reads the disk.
        std::fs::write(root.join("inside.txt"), "changed").expect("write");
        let again = world.compile("#read(\"inside.txt\") ").expect("compiles");
        assert!(first_text(&again).contains("inside"));
        world.clear_file_cache();
        let fresh = world.compile("#read(\"inside.txt\")  ").expect("compiles");
        assert!(first_text(&fresh).contains("changed"));

        let escape = world
            .compile("#read(\"../secret.txt\")")
            .expect_err("denied");
        // The error names the problem; the file's content never appears.
        assert!(!escape.message.is_empty());
        let missing = world.compile("#read(\"nope.txt\")").expect_err("missing");
        assert!(!missing.message.is_empty());
        let invalid = world
            .compile("#read(\"inside.txt\")\n#let _ = read(\"data.bin\")")
            .expect_err("missing");
        assert!(!invalid.message.is_empty());

        // Without a root nothing is readable.
        let sealed = TypstWorld::new(None);
        assert!(sealed.root().is_none());
        assert!(!sealed.file_available("inside.txt"));
        assert!(sealed.compile("#read(\"inside.txt\")").is_err());
        // A root that does not exist behaves like no root.
        let ghost = TypstWorld::new(Some(&outer.join("ghost")));
        assert!(ghost.root().is_none());
        std::fs::remove_dir_all(outer).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_root_are_denied() {
        let outer = temp_dir("link");
        let root = outer.join("doc");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(outer.join("secret.txt"), "secret").expect("write");
        std::os::unix::fs::symlink(outer.join("secret.txt"), root.join("link.txt")).expect("link");
        let world = TypstWorld::new(Some(&root));
        assert!(!world.file_available("link.txt"));
        std::fs::remove_dir_all(&outer).expect("cleanup");
    }

    #[cfg(windows)]
    #[test]
    fn junctions_out_of_the_root_are_denied() {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let outer = temp_dir("junction");
        let root = outer.join("doc");
        let elsewhere = outer.join("elsewhere");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::write(elsewhere.join("secret.txt"), "secret").expect("write");
        std::fs::write(root.join("inside.txt"), "inside").expect("write");
        let link = root.join("link");
        // A junction needs no privilege, unlike a symbolic link.
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&elsewhere)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("cmd runs");
        assert!(made.status.success(), "mklink /J failed: {made:?}");
        // The junction really leads to the file, so the denial below is down to the allow-list.
        assert_eq!(
            std::fs::read(link.join("secret.txt")).expect("via link"),
            b"secret"
        );

        let world = TypstWorld::new(Some(&root));
        assert!(world.file_available("inside.txt"));
        assert!(!world.file_available("link/secret.txt"));
        let denied = world
            .compile("#read(\"link/secret.txt\")")
            .expect_err("denied");
        assert!(!denied.message.is_empty());
        // Remove the junction itself first so the cleanup can never walk through it.
        std::fs::remove_dir(&link).expect("unlink");
        std::fs::remove_dir_all(&outer).expect("cleanup");
    }

    #[test]
    fn invalid_utf8_sources_are_rejected() {
        let world = TypstWorld::new(None);
        world
            .set_virtual_file("bad.typ", vec![0xff, 0xfe, 0x00])
            .expect("valid path");
        let err = world
            .compile("#import \"bad.typ\": *")
            .expect_err("not utf-8");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn packages_are_not_available() {
        let world = TypstWorld::new(None);
        let err = world
            .compile("#import \"@preview/example:0.1.0\": *")
            .expect_err("no network, no packages");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn extra_fonts_are_available_to_documents() {
        let dir = temp_dir("font");
        let file = dir.join("mono.ttf");
        std::fs::write(&file, crate::fonts::BUNDLED_FILES[4]).expect("font");
        let mut world = TypstWorld::new(None);
        assert_eq!(world.add_font_file(&file).expect("added"), 1);
        assert!(world.add_font_file(&dir.join("none.ttf")).is_err());
        assert!(world.font(5).is_some());
        assert!(world.font(6).is_none());
        // New Computer Modern, its Math face and Cascadia Mono (the added copy shares its family).
        assert_eq!(world.book().families().count(), 3);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
