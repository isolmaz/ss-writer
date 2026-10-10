//! Bundled fonts and lazily loaded font files for the Typst world.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use thiserror::Error;
use typst::foundations::Bytes;
use typst::text::{Font, FontBook, FontInfo};
use typst::utils::LazyHash;

/// Family name of the bundled math font.
pub const MATH_FAMILY: &str = "New Computer Modern Math";
/// Family name of the bundled text font (regular, italic and bold).
pub const TEXT_FAMILY: &str = "New Computer Modern";
/// Family name of the bundled monospace font used for code.
pub const MONO_FAMILY: &str = "Cascadia Mono";

static NEWCM_MATH: &[u8] = include_bytes!("../../../assets/fonts/NewCMMath-Regular.otf");
static NEWCM_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/NewCM10-Regular.otf");
static NEWCM_ITALIC: &[u8] = include_bytes!("../../../assets/fonts/NewCM10-Italic.otf");
static NEWCM_BOLD: &[u8] = include_bytes!("../../../assets/fonts/NewCM10-Bold.otf");
static CASCADIA_MONO: &[u8] = include_bytes!("../../../assets/fonts/CascadiaMono.ttf");

/// The font files compiled into the executable.
pub const BUNDLED_FILES: [&[u8]; 5] = [
    NEWCM_MATH,
    NEWCM_REGULAR,
    NEWCM_ITALIC,
    NEWCM_BOLD,
    CASCADIA_MONO,
];

/// Why a font file could not be added.
#[derive(Debug, Error)]
pub enum FontError {
    #[error("cannot read font file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} contains no usable font face")]
    NoFaces { path: PathBuf },
}

/// Parsed bundled faces, shared by every [`FontSet`] (parsing the 3.7 MB of fonts happens once).
fn bundled_fonts() -> &'static [Font] {
    static FONTS: LazyLock<Vec<Font>> = LazyLock::new(|| {
        BUNDLED_FILES
            .iter()
            .flat_map(|data| Font::iter(Bytes::new(*data)))
            .collect()
    });
    &FONTS
}

enum Slot {
    Loaded(Font),
    /// A face of a font file on disk: only its metadata is known until a document uses it.
    Lazy {
        path: PathBuf,
        index: u32,
        font: OnceLock<Option<Font>>,
    },
}

/// The faces available to a Typst world: the bundled fonts first, then optional font files that
/// are parsed on first use.
pub struct FontSet {
    slots: Vec<Slot>,
    book: LazyHash<FontBook>,
}

impl std::fmt::Debug for FontSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontSet")
            .field("faces", &self.slots.len())
            .finish()
    }
}

impl Default for FontSet {
    fn default() -> Self {
        Self::bundled()
    }
}

impl FontSet {
    /// Only the bundled fonts.
    pub fn bundled() -> Self {
        let fonts = bundled_fonts();
        Self {
            slots: fonts.iter().cloned().map(Slot::Loaded).collect(),
            book: LazyHash::new(FontBook::from_fonts(fonts.iter())),
        }
    }

    /// Number of faces (bundled and added).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    /// Adds every face of the font file at `path` (a `.ttf`/`.otf`/`.ttc`). Only the metadata is
    /// read now; a face is parsed when a document first selects it. Returns the number of faces.
    pub fn add_file(&mut self, path: &Path) -> Result<usize, FontError> {
        let data = std::fs::read(path).map_err(|source| FontError::Read {
            path: path.to_owned(),
            source,
        })?;
        let infos: Vec<FontInfo> = FontInfo::iter(&data).collect();
        if infos.is_empty() {
            return Err(FontError::NoFaces {
                path: path.to_owned(),
            });
        }
        let count = infos.len();
        let mut book = FontBook::from_infos(self.book_infos());
        for (index, info) in infos.into_iter().enumerate() {
            book.push(info);
            self.slots.push(Slot::Lazy {
                path: path.to_owned(),
                // A collection holds far fewer than 2^32 faces.
                index: index as u32,
                font: OnceLock::new(),
            });
        }
        self.book = LazyHash::new(book);
        Ok(count)
    }

    fn book_infos(&self) -> Vec<FontInfo> {
        (0..self.slots.len())
            .filter_map(|i| self.book.info(i).cloned())
            .collect()
    }

    /// The face with the given book index, loading it if necessary.
    pub fn font(&self, index: usize) -> Option<Font> {
        match self.slots.get(index)? {
            Slot::Loaded(font) => Some(font.clone()),
            Slot::Lazy { path, index, font } => font
                .get_or_init(|| {
                    let data = std::fs::read(path).ok()?;
                    Font::new(Bytes::new(data), *index)
                })
                .clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typst::text::{FontStretch, FontStyle, FontVariant, FontWeight};

    fn regular() -> FontVariant {
        FontVariant::new(FontStyle::Normal, FontWeight::REGULAR, FontStretch::NORMAL)
    }

    #[test]
    fn bundled_faces_resolve_by_family() {
        let set = FontSet::bundled();
        assert_eq!(set.len(), 5);
        assert!(!set.is_empty());
        for family in [TEXT_FAMILY, MATH_FAMILY, MONO_FAMILY] {
            let index = set
                .book()
                .select(&family.to_lowercase(), regular())
                .unwrap_or_else(|| panic!("{family} missing"));
            let font = set.font(index).expect("face loads");
            assert_eq!(font.info().family.to_lowercase(), family.to_lowercase());
        }
        assert!(set.font(99).is_none());
        assert!(format!("{set:?}").contains("5"));
        assert_eq!(FontSet::default().len(), 5);
    }

    #[test]
    fn add_file_registers_faces_lazily() {
        let dir = std::env::temp_dir().join(format!("ssw-render-fonts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("copy.ttf");
        std::fs::write(&file, CASCADIA_MONO).expect("write font");

        let mut set = FontSet::bundled();
        assert_eq!(set.add_file(&file).expect("added"), 1);
        assert_eq!(set.len(), 6);
        let font = set.font(5).expect("lazy face loads");
        assert_eq!(font.info().family.to_lowercase(), "cascadia mono");
        // Second access hits the cached face.
        assert!(set.font(5).is_some());

        // The file disappears after registration: the face is simply unavailable.
        let mut set2 = FontSet::bundled();
        set2.add_file(&file).expect("added");
        std::fs::remove_file(file).expect("remove");
        assert!(set2.font(5).is_none());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn add_file_reports_errors() {
        let dir = std::env::temp_dir().join(format!("ssw-render-fonts-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut set = FontSet::bundled();
        let missing = set
            .add_file(&dir.join("nope.ttf"))
            .expect_err("missing file");
        assert!(matches!(missing, FontError::Read { .. }));
        assert!(missing.to_string().contains("nope.ttf"));
        let junk = dir.join("junk.ttf");
        std::fs::write(&junk, b"not a font").expect("write");
        let err = set.add_file(&junk).expect_err("not a font");
        assert!(matches!(err, FontError::NoFaces { .. }));
        assert!(err.to_string().contains("junk.ttf"));
        assert_eq!(set.len(), 5);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
