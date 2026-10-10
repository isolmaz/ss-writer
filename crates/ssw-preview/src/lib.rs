//! Explorer preview handler for Markdown files.
//!
//! The DLL is a COM in-process server (`DllGetClassObject`, `DllCanUnloadNow`) for the CLSID in
//! [`factory::CLSID_PREVIEW_HANDLER`]. Explorer's preview pane creates one
//! [`handler`] object per previewed file, hands it the file's stream and a host window, and the
//! handler shows the document read-only (`ssw_ui::editor::EditorView`) in a child window of the
//! host, drawn with Direct2D through DirectComposition.
//!
//! # Modules
//!
//! - [`factory`]: the class factory and the exported COM entry points.
//! - [`handler`]: the preview handler object (`IPreviewHandler` and friends) and the helpers
//!   that inspect it in tests.
//! - [`server`]: the object and lock counts behind `DllCanUnloadNow`.
//! - [`source`]: reads Explorer's `IStream` with a size cap and decodes the bytes to text.
//! - [`appearance`]: the colors and font Explorer asks for (`IPreviewHandlerVisuals`) laid over
//!   the system theme.
//! - [`notice`]: the localized text shown instead of a document that cannot be previewed.
//!
//! The rest is internal: the document view (`content`), its math and diagram objects
//! (`objects`) and the child window (`window`).

mod content;
mod objects;
mod window;

pub mod appearance;
pub mod factory;
pub mod handler;
pub mod notice;
pub mod server;
pub mod source;
