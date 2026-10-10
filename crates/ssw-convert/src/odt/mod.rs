//! OpenDocument text: import ([`import_odt`]) and export ([`export_odt`]).

mod package;
mod read;
mod slug;
mod styles;
mod write;
mod xml;

pub use read::import_odt;
pub use write::{ExportError, export_odt};
