//! Saving (spec §4.3): atomic text writes in the document's own encoding, "Save as" including
//! the word-processor and bundle formats, and "Save back to Word / ODT / RTF" with the version
//! snapshot taken first.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ssw_convert::assets::{AssetProvider, image_mime_for_path, is_external_url};
use ssw_convert::docx::{ExportOptions, MathImage, Resources};
use ssw_convert::import::Asset;
use ssw_convert::textbundle;
use ssw_io::encoding::{self, TextFormat};
use ssw_io::fs::FileSystem;
use ssw_io::history::SnapshotReason;
use ssw_io::save::{SaveStamp, Sleeper, WriteAccess, save_atomic};

use crate::session::remember_language;

use super::{
    ConvertedFormat, DocEntry, DocError, Documents, ExternalState, Source, canonical_path,
};

/// What a successful save wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The file (or bundle folder) written.
    pub path: PathBuf,
    /// Identity of the content on disk after the write.
    pub stamp: SaveStamp,
}

/// A sleeper that returns at once: a save made with it tries again right away after a sharing
/// violation instead of waiting for the other program to let go.
#[derive(Debug)]
struct NoSleep;

impl Sleeper for NoSleep {
    fn sleep(&self, _duration: Duration) {}
}

/// Images and math for exports: the importer's extracted images first, then files next to the
/// document. Math and Mermaid are not rendered here.
struct LocalAssets<'a> {
    fs: &'a dyn FileSystem,
    base_dir: &'a Path,
    assets: &'a [Asset],
}

impl LocalAssets<'_> {
    fn read(&self, destination: &str) -> Option<Vec<u8>> {
        if is_external_url(destination) {
            return None;
        }
        if let Some(asset) = self
            .assets
            .iter()
            .find(|asset| asset.relative_path == destination)
        {
            return Some(asset.bytes.clone());
        }
        let relative: PathBuf = destination.split('/').collect();
        self.fs.read(&self.base_dir.join(relative)).ok()
    }
}

impl AssetProvider for LocalAssets<'_> {
    fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
        let mime = image_mime_for_path(path)?;
        Some((self.read(path)?, mime))
    }

    fn math_svg(&self, _latex: &str, _display: bool) -> Option<String> {
        None
    }

    fn mermaid_svg(&self, _source: &str) -> Option<String> {
        None
    }
}

/// A failed conversion step (an exporter, the TextBundle writer) by its message.
fn conversion_failed(error: impl ToString) -> DocError {
    DocError::Convert(error.to_string())
}

/// The bytes of `markdown` in the word-processor format `format`.
fn export_bytes(
    format: ConvertedFormat,
    markdown: &str,
    title: &str,
    provider: &LocalAssets<'_>,
) -> Result<Vec<u8>, DocError> {
    let options = ExportOptions {
        title: Some(title.to_owned()),
        ..ExportOptions::default()
    };
    let mut image = |destination: &str| provider.read(destination);
    let mut math = |_: &str, _: bool| -> Option<MathImage> { None };
    match format {
        ConvertedFormat::Docx => {
            let mut resources = Resources {
                image: &mut image,
                math: &mut math,
            };
            ssw_convert::docx::export_docx(markdown, &options, &mut resources)
                .map_err(conversion_failed)
        }
        ConvertedFormat::Odt => {
            let mut resources = Resources {
                image: &mut image,
                math: &mut math,
            };
            ssw_convert::odt::export_odt(markdown, &options, &mut resources)
                .map_err(conversion_failed)
        }
        ConvertedFormat::Rtf => {
            Ok(ssw_convert::rtf::markdown_to_rtf(markdown, provider).into_bytes())
        }
        ConvertedFormat::Doc => Err(DocError::Unsupported("Word 97-2003")),
    }
}

fn title_of(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Documents {
    /// Saves a document to its file, in its own encoding and line endings, atomically. A file
    /// that another program holds is tried again after 50, 100, 200, 400 and 800 ms.
    ///
    /// # Errors
    /// [`DocError::Untitled`] for a buffer without a file, [`DocError::NeedsSaveAs`] for a
    /// converted document, [`DocError::Encode`] when the encoding cannot represent the text
    /// (offer UTF-8), [`DocError::Save`] for write failures (read-only: offer "Allow editing").
    pub fn save(&self, entry: &DocEntry) -> Result<Saved, DocError> {
        self.save_waiting(entry, self.env.sleeper.as_ref())
    }

    /// [`Documents::save`] for a caller that must not stall the UI (the autosave tick): it never
    /// sleeps. A file that another program holds is tried again at once instead of after the
    /// backoff, and the sharing violation is returned when it lasts; the caller decides when to
    /// try again.
    ///
    /// # Errors
    /// The errors of [`Documents::save`].
    pub fn save_without_waiting(&self, entry: &DocEntry) -> Result<Saved, DocError> {
        self.save_waiting(entry, &NoSleep)
    }

    fn save_waiting(&self, entry: &DocEntry, sleeper: &dyn Sleeper) -> Result<Saved, DocError> {
        let (path, source) = {
            let state = entry.state.borrow();
            (state.path.clone(), state.source.clone())
        };
        let path = path.ok_or(DocError::Untitled)?;
        let (text, version) = {
            let document = entry.document.borrow();
            (document.text(), document.version())
        };
        let format = entry.state.borrow().format;
        let stamp = match &source {
            Source::Text => {
                let bytes = encoding::encode(&text, &format)?;
                save_atomic(self.env.fs.as_ref(), sleeper, &path, &bytes)?
            }
            Source::TextBundle { text_file } => save_atomic(
                self.env.fs.as_ref(),
                sleeper,
                &path.join(text_file),
                text.as_bytes(),
            )?,
            Source::TextPack => {
                let assets = entry.state.borrow().assets.clone();
                let bytes = textbundle::write_pack(&text, &assets).map_err(conversion_failed)?;
                save_atomic(self.env.fs.as_ref(), sleeper, &path, &bytes)?
            }
            Source::Converted(_) => return Err(DocError::NeedsSaveAs),
        };
        let tracked = !matches!(source, Source::TextBundle { .. });
        self.finish_save(entry, &path, version, stamp, tracked);
        Ok(Saved { path, stamp })
    }

    /// Updates the state after the content at `path` was written.
    fn finish_save(
        &self,
        entry: &DocEntry,
        path: &Path,
        version: u64,
        stamp: SaveStamp,
        tracked: bool,
    ) {
        {
            let mut state = entry.state.borrow_mut();
            state.saved_version = version;
            state.external = ExternalState::InSync;
            state.access = WriteAccess::Writable;
            state.format.had_mixed_endings = false;
            state.lossy = false;
        }
        if tracked {
            self.tracker.borrow_mut().track(path, stamp);
        }
    }

    /// "Save back to Word / ODT / RTF": converts the Markdown text, snapshots it in the version
    /// history and replaces the original file.
    ///
    /// # Errors
    /// [`DocError::Unsupported`] for `.doc` (import only) and for documents that were not
    /// imported, [`DocError::Convert`] when the exporter fails, [`DocError::Store`] when the
    /// snapshot cannot be taken (nothing is replaced then), [`DocError::Save`] for write
    /// failures.
    pub fn save_back(&self, entry: &DocEntry) -> Result<Saved, DocError> {
        let (path, source) = {
            let state = entry.state.borrow();
            (state.path.clone(), state.source.clone())
        };
        let path = path.ok_or(DocError::Untitled)?;
        let Source::Converted(format) = source else {
            return Err(DocError::Unsupported("non-converted"));
        };
        let (text, version) = {
            let document = entry.document.borrow();
            (document.text(), document.version())
        };
        let assets = entry.state.borrow().assets.clone();
        let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let provider = LocalAssets {
            fs: self.env.fs.as_ref(),
            base_dir: &base_dir,
            assets: &assets,
        };
        let bytes = export_bytes(format, &text, &title_of(&path), &provider)?;
        self.snapshot(entry, SnapshotReason::BeforeSaveBack)?;
        let stamp = save_atomic(
            self.env.fs.as_ref(),
            self.env.sleeper.as_ref(),
            &path,
            &bytes,
        )?;
        self.finish_save(entry, &path, version, stamp, true);
        Ok(Saved { path, stamp })
    }

    /// Writes the files an importer or bundle extracted next to `dir`.
    fn write_assets(&self, dir: &Path, assets: &[Asset]) -> Result<(), DocError> {
        for asset in assets {
            let unsafe_part =
                |part: &str| part.is_empty() || part == ".." || part.contains([':', '\\']);
            if asset.relative_path.split('/').any(unsafe_part) {
                continue;
            }
            let relative: PathBuf = asset.relative_path.split('/').collect();
            let target = dir.join(relative);
            // The path has at least one part, so the target always has a parent.
            self.env.fs.create_dir_all(target.parent().unwrap_or(dir))?;
            save_atomic(
                self.env.fs.as_ref(),
                self.env.sleeper.as_ref(),
                &target,
                &asset.bytes,
            )?;
        }
        Ok(())
    }

    /// "Save as": writes the document to `target`, chosen by its extension: `.docx` `.odt`
    /// `.rtf` convert, `.textpack` / `.textbundle` write a bundle, anything else is text. The
    /// document is bound to the new file afterwards (an untitled buffer stops being untitled,
    /// its backups are removed) and the language the user chose for it is remembered for that
    /// file; the file it came from keeps its own choice, as it keeps its own history.
    ///
    /// # Errors
    /// [`DocError::AlreadyOpen`] when another tab shows `target`, [`DocError::Unsupported`] for
    /// `.doc`, [`DocError::Store`] when the language choice cannot be stored (the file is saved
    /// and the document bound anyway), plus the errors of [`Documents::save`].
    pub fn save_as(&self, entry: &DocEntry, target: &Path) -> Result<Saved, DocError> {
        let target = canonical_path(target);
        if let Some(open) = self.find(&target)
            && !std::ptr::eq(open.as_ref(), entry)
        {
            return Err(DocError::AlreadyOpen(target));
        }
        let extension = target
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let (text, version) = {
            let document = entry.document.borrow();
            (document.text(), document.version())
        };
        let (old_source, old_format, assets) = {
            let state = entry.state.borrow();
            (state.source.clone(), state.format, state.assets.clone())
        };
        let fs = self.env.fs.as_ref();
        let sleeper = self.env.sleeper.as_ref();
        let base_dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut new_format = TextFormat::default();
        let mut keep_assets = Vec::new();
        let mut tracked = true;
        let converted = ConvertedFormat::from_extension(&extension);
        let (stamp, new_source) = match (converted, extension.as_str()) {
            // `.doc` is a converted format too: its exporter refuses it.
            (Some(format), _) => {
                let provider = LocalAssets {
                    fs,
                    base_dir: &base_dir,
                    assets: &assets,
                };
                let bytes = export_bytes(format, &text, &title_of(&target), &provider)?;
                (
                    save_atomic(fs, sleeper, &target, &bytes)?,
                    Source::Converted(format),
                )
            }
            (None, "textpack") => {
                let bytes = textbundle::write_pack(&text, &assets).map_err(conversion_failed)?;
                keep_assets = assets.clone();
                (save_atomic(fs, sleeper, &target, &bytes)?, Source::TextPack)
            }
            (None, "textbundle") => {
                textbundle::write_dir(&target, &text, &assets).map_err(conversion_failed)?;
                tracked = false;
                let meta = fs.metadata(&target)?;
                (
                    SaveStamp::from_read(&meta, text.as_bytes()),
                    Source::TextBundle {
                        text_file: "text.markdown".to_owned(),
                    },
                )
            }
            (None, _) => {
                if matches!(old_source, Source::Text) {
                    new_format = old_format;
                }
                let bytes = encoding::encode(&text, &new_format)?;
                let stamp = save_atomic(fs, sleeper, &target, &bytes)?;
                self.write_assets(&base_dir, &assets)?;
                (stamp, Source::Text)
            }
        };
        let untitled_id = entry.untitled_id();
        {
            let mut state = entry.state.borrow_mut();
            if let Some(old) = state.path.take() {
                self.tracker.borrow_mut().untrack(&old);
            }
            state.path = Some(target.clone());
            state.untitled = None;
            state.source = new_source;
            state.format = new_format;
            state.assets = keep_assets;
            state.losses.clear();
        }
        if let Some(id) = untitled_id {
            self.drop_untitled_backups(&id);
        }
        self.finish_save(entry, &target, version, stamp, tracked);
        remember_language(&self.env.store.borrow(), &target, entry.language_choice())?;
        Ok(Saved {
            path: target,
            stamp,
        })
    }
}

#[cfg(test)]
mod tests {
    use ssw_io::fs::RealFs;

    use super::*;

    #[test]
    fn images_come_from_the_importer_first_then_from_next_to_the_document() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("img")).unwrap();
        std::fs::write(tmp.path().join("img").join("disk.png"), b"disk").unwrap();
        std::fs::write(tmp.path().join("plain.JPG"), b"jpeg").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"text").unwrap();
        let assets = [Asset {
            relative_path: "img/disk.png".to_owned(),
            bytes: b"imported".to_vec(),
        }];
        let provider = LocalAssets {
            fs: &RealFs,
            base_dir: tmp.path(),
            assets: &assets,
        };
        // The importer's copy wins over the file of the same name.
        assert_eq!(
            provider.image("img/disk.png"),
            Some((b"imported".to_vec(), "image/png"))
        );
        // Anything else is read below the document's folder, the MIME type from the extension.
        assert_eq!(
            provider.image("plain.JPG"),
            Some((b"jpeg".to_vec(), "image/jpeg"))
        );
        assert_eq!(provider.image("gone.png"), None);
        // Files that are no images and addresses on the web are never read.
        assert_eq!(provider.image("notes.txt"), None);
        assert_eq!(provider.read("notes.txt"), Some(b"text".to_vec()));
        assert_eq!(provider.read("https://example.com/a.png"), None);
        assert_eq!(provider.image("https://example.com/a.png"), None);
    }

    #[test]
    fn math_and_diagrams_are_not_rendered_for_exports() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalAssets {
            fs: &RealFs,
            base_dir: tmp.path(),
            assets: &[],
        };
        assert_eq!(provider.math_svg("x^2", false), None);
        assert_eq!(provider.math_svg("x^2", true), None);
        assert_eq!(provider.mermaid_svg("graph TD\n  A --> B"), None);
    }

    #[test]
    fn every_word_processor_format_has_its_own_package_and_doc_cannot_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalAssets {
            fs: &RealFs,
            base_dir: tmp.path(),
            assets: &[],
        };
        let export = |format| export_bytes(format, "# Title\n\nBody $x$\n", "Title", &provider);
        assert!(export(ConvertedFormat::Docx).unwrap().starts_with(b"PK"));
        assert!(export(ConvertedFormat::Odt).unwrap().starts_with(b"PK"));
        assert!(export(ConvertedFormat::Rtf).unwrap().starts_with(b"{\\rtf"));
        assert_eq!(
            export(ConvertedFormat::Doc).unwrap_err().message_id(),
            "app-error-unsupported"
        );
    }
}
