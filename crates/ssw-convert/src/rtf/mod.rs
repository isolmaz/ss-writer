//! RTF export ([`write`]) and import ([`read`]).

pub mod read;
pub mod write;

pub use read::import_rtf;
pub use write::markdown_to_rtf;
