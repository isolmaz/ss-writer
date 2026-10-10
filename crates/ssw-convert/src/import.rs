//! Shared result model for every importer (DOCX, ODT, RTF, DOC, HTML).
//!
//! Importers produce Markdown text plus extracted assets and a list of what could not be
//! represented. The editor shows `losses` in a banner (spec §4.1).

/// A binary asset extracted from an imported document (images, embedded files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// Path relative to the Markdown file, using `/` separators (e.g. `report_assets/image-1.png`).
    pub relative_path: String,
    /// Raw file bytes.
    pub bytes: Vec<u8>,
}

/// Category of content an importer could not represent in Markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LossKind {
    MergedCells,
    HeadersFooters,
    Columns,
    TextBoxes,
    Comments,
    TrackedChanges,
    VectorShapes,
    UnsupportedEquations,
    UnsupportedImages,
    Alignment,
    Underline,
    FontsAndColors,
    SubSuperscript,
    NestedTables,
    MultiParagraphCells,
    Fields,
    HiddenText,
    SymbolFontCharacters,
    ListNumberingStyle,
    SectionBreaks,
    CorruptParts,
}

/// One aggregated loss entry: the kind, how many occurrences, and an optional human detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loss {
    pub kind: LossKind,
    pub count: usize,
    pub detail: Option<String>,
}

/// Output of an importer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportResult {
    pub markdown: String,
    pub assets: Vec<Asset>,
    pub losses: Vec<Loss>,
}

impl ImportResult {
    /// Records `count` occurrences of `kind`, merging with an existing entry of the same kind.
    pub fn record_loss(&mut self, kind: LossKind, count: usize, detail: Option<String>) {
        if count == 0 {
            return;
        }
        if let Some(existing) = self.losses.iter_mut().find(|l| l.kind == kind) {
            existing.count += count;
            if existing.detail.is_none() {
                existing.detail = detail;
            }
        } else {
            self.losses.push(Loss {
                kind,
                count,
                detail,
            });
        }
    }

    /// Losses sorted by kind for stable presentation.
    pub fn sorted_losses(&self) -> Vec<Loss> {
        let mut v = self.losses.clone();
        v.sort_by_key(|l| l.kind);
        v
    }
}

/// Why an importer refused a file.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImportError {
    #[error("the document is encrypted")]
    Encrypted,
    #[error("unsupported format version: {0}")]
    UnsupportedVersion(String),
    #[error("the file is not a {expected} document (looks like {detected})")]
    WrongFormat {
        expected: &'static str,
        detected: &'static str,
    },
    #[error("the document is corrupt: {0}")]
    Corrupt(String),
    #[error("the document exceeds the size limit: {0}")]
    TooLarge(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn losses_merge_by_kind_and_ignore_zero() {
        let mut r = ImportResult::default();
        r.record_loss(LossKind::Comments, 2, None);
        r.record_loss(LossKind::MergedCells, 0, Some("x".into()));
        r.record_loss(LossKind::Comments, 3, Some("in body".into()));
        r.record_loss(LossKind::Alignment, 1, None);
        assert_eq!(r.losses.len(), 2);
        let sorted = r.sorted_losses();
        assert_eq!(sorted[0].kind, LossKind::Comments);
        assert_eq!(sorted[0].count, 5);
        assert_eq!(sorted[0].detail.as_deref(), Some("in body"));
        assert_eq!(sorted[1].kind, LossKind::Alignment);
    }
}
