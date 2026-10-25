//! Text Services Framework: the editor as a TSF text store.
//!
//! TSF is how Windows delivers text that does not come from keys: IME compositions (Japanese,
//! Chinese, Korean), dictation (`Win+H`), the emoji panel (`Win+.`), handwriting and the touch
//! keyboard, and how Windows reads text back for reconversion. The application owns one
//! document manager per window with one context whose text store is [`TextStore`].
//!
//! # Pieces
//!
//! - [`TextStoreHost`]: the editor side. The store reads and edits the document through it,
//!   in the editor's UTF-8 byte offsets; the store converts to and from TSF's UTF-16 positions
//!   (ACPs) with [`ssw_core::text::Utf16Map`].
//! - [`TextStore`]: the `ITextStoreACP2` COM object (plus the composition and text-edit sinks)
//!   over a host: lock management, sink advise/unadvise, change notifications, layout queries
//!   (`GetTextExt`, `GetScreenExt`, `GetACPFromPoint`), the plain-text input scope.
//! - [`TextServices`]: the thread manager activation, document manager, context and focus
//!   association of one window around a [`TextStore`].
//!
//! # Composition
//!
//! While an IME composes, TSF writes the composition string into the store. The string is part
//! of the text TSF sees but not of the host document: the host shows it as an overlay at the
//! selection and learns about it through [`ssw_ui::input::ImeEvent`]s delivered to
//! [`TextStoreHost::ime`], the same events the widget kit consumes:
//!
//! - `Start`: a composition began. The host removes the selected text (the composition replaces
//!   it) and starts an empty overlay. The store sets the host selection to the text the
//!   composition replaces first.
//! - `Update`: the composition string, its caret and its display attributes changed. Sent after
//!   TSF released the document lock, once per edit.
//! - `Commit(text)`, `End`: the composition ended; the host inserts `text` verbatim at the
//!   selection and drops the overlay. A cancelled composition sends `End` alone.
//!
//! # Threading and re-entrancy
//!
//! Everything runs on the UI thread (a single-threaded apartment). TSF calls the store from
//! inside its own calls (`GetText` from inside `OnLockGranted`, `RequestLock` from inside
//! `OnTextChange`), and the store calls the host from inside those. Consequently:
//!
//! - Host methods take `&self`; hosts use interior mutability and must not hold a borrow of
//!   their own state while they call the notification methods of [`TextStore`].
//! - Notifications the host raises from inside a host method that the store itself called
//!   (for example `text_changed` from inside [`TextStoreHost::replace`]) are ignored: they are
//!   echoes of an edit TSF requested and already knows.
//! - Notifications are deferred while TSF holds the document lock and delivered, merged, right
//!   after it is released.
//!
//! # Cost
//!
//! The store caches the document as UTF-16 with a block table that converts byte and UTF-16
//! offsets. It is built from [`TextStoreHost::text`] once, when TSF first reads, and then
//! follows every edit incrementally (the `removed` / `inserted` text of
//! [`TextStore::text_changed`] and the edits TSF itself requests), so an edit costs
//! `O(edit + document / 4096)`. Host edits not reported through `text_changed` are not seen; a
//! host that shows another document reports it with [`TextServices::document_replaced`], which
//! drops the cache and any composition.

mod com;
mod session;
mod store;
mod view;

use std::ops::Range;

use ssw_core::edit::Selection;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::ImeEvent;

pub use com::TextStore;
pub use session::TextServices;

/// The editor as TSF sees it. Offsets are UTF-8 byte offsets into the document, always on char
/// boundaries; rectangles and points are window DIPs (client area), converted to screen pixels
/// by the store. All methods take `&self` (see the module documentation on re-entrancy).
pub trait TextStoreHost {
    /// The whole document text.
    fn text(&self) -> String;

    /// The selection: `anchor` is where it started, `head` is the caret.
    fn selection(&self) -> Selection;

    /// Selects `selection`. Called for edits TSF requested; the host must not notify the store.
    fn set_selection(&self, selection: Selection);

    /// Replaces `range` by `text` verbatim (no typing behaviours such as auto-pairing) and maps
    /// the selection through the edit. Returns false if the edit was refused.
    fn replace(&self, range: Range<usize>, text: &str) -> bool;

    /// True when the document cannot be edited; TSF is told so and clients stop editing.
    fn is_read_only(&self) -> bool;

    /// The visible text area, or `None` while there is no layout (window hidden or minimized).
    fn view_rect(&self) -> Option<Rect>;

    /// The bounding rectangle of the text `range`, or `None` if it has no layout. An empty
    /// range is the caret rectangle at that offset.
    fn range_rect(&self, range: Range<usize>) -> Option<Rect>;

    /// The bounding rectangle of `range` (UTF-8 offsets inside the composition string) of the
    /// composition overlay; the candidate window is placed from it.
    fn composition_rect(&self, range: Range<usize>) -> Option<Rect>;

    /// The document offset of the character at `point`, if the point is over the text.
    fn offset_at_point(&self, point: Point) -> Option<usize>;

    /// An IME composition event; see the module documentation.
    fn ime(&self, event: ImeEvent);
}
