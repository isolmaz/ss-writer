//! Markdown: the shared tree ([`ast`]), the reference parser ([`parse_document`]) and the
//! document-level definition tables ([`definitions`]).
//!
//! # DEVIATIONS
//!
//! The parser follows CommonMark 0.31.2 plus the GFM extensions listed in the product
//! specification. It differs from the reference implementations in these deliberate ways; the
//! conformance test (`tests/commonmark.rs`) lists every example affected in
//! `tests/data/expected-failures.txt`.
//!
//! * **No Setext headings.** `Foo\n---` is a paragraph followed by a thematic break and
//!   `Foo\n===` is a two-line paragraph. Only ATX headings exist.
//! * **Raw HTML is kept, never rendered.** HTML blocks and inline HTML are nodes carrying their
//!   source; no HTML is interpreted, filtered or sanitized by the parser.
//! * **`~x~` is underline**, `~~x~~` is strikethrough (GFM treats a single tilde as
//!   strikethrough). Runs of three or more tildes are literal text.
//! * **`==x==` is highlight** (`=={red}x==` colored); `$x$` / `$$x$$` are math; `[[x]]` is a
//!   wikilink even when a reference definition `[[x]]` exists.
//! * **Task items, callouts, footnotes, front matter and `<!-- pagebreak -->`** are recognized as
//!   dedicated nodes where CommonMark sees a paragraph, a block quote or an HTML block.
//! * **Line endings:** only `\n` and `\r\n` end a line; a lone `\r` is an ordinary character.
//! * Tables follow GFM; a table needs a pipe in its delimiter row or header row.

pub mod ast;
pub mod definitions;
mod entities;
pub mod incremental;
mod parse;
mod unicode_punct;

pub use parse::{parse_document, unescape_markdown};
