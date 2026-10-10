//! DOCX import (`import_docx`) and export (`export_docx`).

pub mod numbering;
mod read;
pub mod styles;
pub mod write;

pub use read::import_docx;
pub use write::{ExportError, ExportOptions, MathImage, PageSize, Resources, export_docx};
