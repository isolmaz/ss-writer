//! Document statistics (spec §8): words, characters, paragraphs, sentences, reading time.
//!
//! Everything is measured on the *visible text* of the Markdown tree, not on the source:
//!
//! * Counted text: paragraphs, headings, callout titles, table cells, code (spans and blocks).
//!   Link text, image alt text and the alias (or target) of a wikilink are visible; escapes and
//!   entities count as the character they stand for; soft and hard line breaks count as one
//!   space.
//! * Not counted: syntax markers, link and image destinations, autolink URLs, footnote
//!   references, inline and display math, raw HTML, front matter, link reference definitions.
//! * **Words** follow Unicode word segmentation (UAX #29) and only segments containing a letter
//!   or digit count, so every CJK ideograph is a word of its own.
//! * **Characters** are grapheme clusters; the "no spaces" count skips whitespace-only
//!   clusters. Line breaks between blocks are not characters.
//! * **Paragraphs** are the non-empty paragraphs and headings anywhere in the tree (list items,
//!   quotes, callouts and footnotes included); code, tables, math and HTML are not paragraphs.
//! * **Sentences** are UAX #29 sentences of those paragraphs and headings.
//!
//! [`for_range`] applies the same rules to the part of the text a selection covers.
//!
//! Plain-text documents have no tree: [`compute_plain`] and [`for_range_plain`] apply the same
//! word, character and sentence rules to the raw text, where nothing is syntax. A paragraph is
//! a run of non-blank lines; blank (whitespace-only) lines separate paragraphs and are not
//! counted, a line break inside a paragraph counts as one space.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;

use crate::markdown::ast::{BlockKind, Document, Inline, InlineKind, LinkKind};
use crate::text::TextRange;
use crate::toc::wikilink_text;

/// Counts for a document or a selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub words: usize,
    /// Visible characters including spaces.
    pub characters: usize,
    /// Visible characters without whitespace.
    pub characters_no_spaces: usize,
    pub paragraphs: usize,
    pub sentences: usize,
}

impl Stats {
    /// Estimated reading time in whole minutes at `wpm` words per minute: the ceiling of
    /// `words / wpm`, so any text takes at least one minute and an empty document none. A
    /// speed of zero is treated as one word per minute.
    pub fn reading_minutes(&self, wpm: u32) -> u32 {
        let minutes = self.words.div_ceil(wpm.max(1) as usize);
        u32::try_from(minutes).unwrap_or(u32::MAX)
    }
}

/// Statistics of the whole document; `doc` was parsed from `src`.
pub fn compute(src: &str, doc: &Document) -> Stats {
    collect(src, doc, None)
}

/// Statistics of the text covered by `selection`. Partially selected text counts only the
/// selected part; syntax, URLs and the other excluded content stay excluded. An empty
/// selection yields zeros. Offsets inside a character are widened to the whole character.
pub fn for_range(src: &str, doc: &Document, selection: TextRange) -> Stats {
    let start = floor_boundary(src, selection.start);
    let end = ceil_boundary(src, selection.end);
    collect(src, doc, Some(TextRange::new(start, end.max(start))))
}

/// Statistics of a plain-text document (no Markdown interpretation).
pub fn compute_plain(src: &str) -> Stats {
    let mut stats = Stats::default();
    for paragraph in plain_paragraphs(src) {
        // A line break inside a paragraph is a space (and must not end a sentence).
        add_text(&mut stats, &paragraph.replace('\n', " "), true);
    }
    stats
}

/// Statistics of the part of a plain-text document covered by `selection` (offsets inside a
/// character are widened to the whole character; an empty selection yields zeros).
pub fn for_range_plain(src: &str, selection: TextRange) -> Stats {
    let start = floor_boundary(src, selection.start);
    let end = ceil_boundary(src, selection.end).max(start);
    compute_plain(&src[start..end])
}

/// The maximal runs of non-blank lines of `src`, each from the start of its first line to the
/// end of its last line (without the final line break).
fn plain_paragraphs(src: &str) -> Vec<&str> {
    let mut paragraphs = Vec::new();
    let mut run: Option<(usize, usize)> = None;
    let mut offset = 0;
    for line in src.split('\n') {
        let end = offset + line.len();
        if line.trim().is_empty() {
            if let Some((start, stop)) = run.take() {
                paragraphs.push(&src[start..stop]);
            }
        } else {
            run = Some(run.map_or((offset, end), |(start, _)| (start, end)));
        }
        offset = end + 1;
    }
    if let Some((start, stop)) = run {
        paragraphs.push(&src[start..stop]);
    }
    paragraphs
}

fn floor_boundary(s: &str, offset: usize) -> usize {
    let mut i = offset.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, offset: usize) -> usize {
    let mut i = offset.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// A run of visible text and the source range it stands for.
struct Piece<'a> {
    range: TextRange,
    text: Cow<'a, str>,
    /// `text` is exactly the source text of `range` and can be cut at any boundary; other
    /// pieces (entities, code spans, breaks…) are all-or-nothing.
    exact: bool,
}

/// Pieces that form one unit for word counting: words never span groups.
struct Group<'a> {
    /// Counts as a paragraph and contributes sentences.
    prose: bool,
    pieces: Vec<Piece<'a>>,
}

fn collect(src: &str, doc: &Document, selection: Option<TextRange>) -> Stats {
    let mut stats = Stats::default();
    for block in doc.iter_blocks() {
        if selection.is_some_and(|s| !block.range.intersects(s)) {
            continue;
        }
        let mut groups: Vec<Group<'_>> = Vec::new();
        match &block.kind {
            BlockKind::Heading { .. } | BlockKind::Paragraph => groups.push(Group {
                prose: true,
                pieces: inline_pieces(&block.inlines, src),
            }),
            BlockKind::Callout {
                title: Some(title), ..
            } => groups.push(Group {
                prose: true,
                pieces: vec![exact_piece(*title, src)],
            }),
            BlockKind::CodeBlock(code) => {
                let mut pieces = Vec::new();
                let mut previous_end = None;
                for line in &code.lines {
                    if let Some(end) = previous_end {
                        // The line break between two lines is its own all-or-nothing piece.
                        pieces.push(Piece {
                            range: TextRange::new(end, end + 1),
                            text: Cow::Borrowed("\n"),
                            exact: false,
                        });
                    }
                    pieces.push(exact_piece(line.range, src));
                    previous_end = Some(line.range.end);
                }
                groups.push(Group {
                    prose: false,
                    pieces,
                });
            }
            BlockKind::Table(table) => {
                let cells = table
                    .header
                    .cells
                    .iter()
                    .chain(table.rows.iter().flat_map(|row| row.cells.iter()));
                for cell in cells {
                    groups.push(Group {
                        prose: false,
                        pieces: inline_pieces(&cell.inlines, src),
                    });
                }
            }
            _ => {}
        }
        for group in &groups {
            tally(&mut stats, group, selection);
        }
    }
    stats
}

fn exact_piece(range: TextRange, src: &str) -> Piece<'_> {
    Piece {
        range,
        text: Cow::Borrowed(range.get(src).unwrap_or("")),
        exact: true,
    }
}

fn inline_pieces<'a>(inlines: &'a [Inline], src: &'a str) -> Vec<Piece<'a>> {
    let mut pieces = Vec::new();
    push_pieces(inlines, src, &mut pieces);
    pieces
}

fn push_pieces<'a>(inlines: &'a [Inline], src: &'a str, out: &mut Vec<Piece<'a>>) {
    let atom = |inline: &Inline, text: &'a str| Piece {
        range: inline.range,
        text: Cow::Borrowed(text),
        exact: false,
    };
    for inline in inlines {
        match &inline.kind {
            InlineKind::Text => out.push(exact_piece(inline.range, src)),
            InlineKind::Escape => out.push(atom(inline, inline.content.get(src).unwrap_or(""))),
            InlineKind::Entity { decoded } => out.push(atom(inline, decoded)),
            InlineKind::Code { text } => out.push(atom(inline, text)),
            InlineKind::WikiLink(link) => out.push(atom(inline, wikilink_text(link))),
            InlineKind::SoftBreak | InlineKind::HardBreak => out.push(atom(inline, " ")),
            InlineKind::Link(link) if matches!(link.kind, LinkKind::Autolink { .. }) => {}
            InlineKind::Emphasis
            | InlineKind::Strong
            | InlineKind::Underline
            | InlineKind::Strikethrough
            | InlineKind::Highlight { .. }
            | InlineKind::Link(_)
            | InlineKind::Image(_) => push_pieces(&inline.children, src, out),
            InlineKind::FootnoteRef { .. } | InlineKind::InlineMath | InlineKind::RawHtml => {}
        }
    }
}

/// Adds the contribution of `group` (restricted to `selection`) to `stats`.
fn tally(stats: &mut Stats, group: &Group<'_>, selection: Option<TextRange>) {
    let mut text = String::new();
    for piece in &group.pieces {
        match selection {
            None => text.push_str(&piece.text),
            Some(sel) => {
                let Some(shared) = piece.range.intersection(sel) else {
                    continue;
                };
                if piece.exact {
                    let from = shared.start - piece.range.start;
                    let to = shared.end - piece.range.start;
                    text.push_str(piece.text.get(from..to).unwrap_or(""));
                } else {
                    text.push_str(&piece.text);
                }
            }
        }
    }
    add_text(stats, &text, group.prose);
}

/// Adds the words and characters of `text`; a `prose` text that is not blank also counts as a
/// paragraph and contributes its sentences.
fn add_text(stats: &mut Stats, text: &str, prose: bool) {
    stats.words += text.unicode_words().count();
    for cluster in text.graphemes(true) {
        stats.characters += 1;
        if !cluster.chars().all(char::is_whitespace) {
            stats.characters_no_spaces += 1;
        }
    }
    if prose && !text.trim().is_empty() {
        stats.paragraphs += 1;
        stats.sentences += text.unicode_sentences().count();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse_document;

    fn stats(src: &str) -> Stats {
        compute(src, &parse_document(src))
    }

    fn selected(src: &str, from: &str, to: &str) -> Stats {
        let start = src.find(from).unwrap();
        let end = src.find(to).unwrap() + to.len();
        for_range(src, &parse_document(src), TextRange::new(start, end))
    }

    fn s(
        words: usize,
        characters: usize,
        characters_no_spaces: usize,
        paragraphs: usize,
        sentences: usize,
    ) -> Stats {
        Stats {
            words,
            characters,
            characters_no_spaces,
            paragraphs,
            sentences,
        }
    }

    #[test]
    fn empty_document_is_all_zero() {
        assert_eq!(stats(""), Stats::default());
        assert_eq!(stats("\n\n"), Stats::default());
        assert_eq!(stats("").reading_minutes(200), 0);
    }

    #[test]
    fn plain_paragraph() {
        assert_eq!(stats("Hello world"), s(2, 11, 10, 1, 1));
        assert_eq!(stats("Hello world.\n"), s(2, 12, 11, 1, 1));
        assert_eq!(stats("Hello world. How are you? Fine!"), s(6, 31, 26, 1, 3));
    }

    #[test]
    fn markers_urls_and_images_are_not_counted() {
        let src = "**Hello** _big_ [world](https://example.com/very/long) ![img alt](pic.png)";
        assert_eq!(stats(src), s(5, 23, 19, 1, 1));
        let urls = "see <https://x.org> and https://y.org ok";
        assert_eq!(stats(urls), s(3, 12, 8, 1, 1));
    }

    #[test]
    fn front_matter_math_html_and_definitions_are_skipped() {
        let src = "---\ntitle: skipped words\n---\n\nBody text\n\n$$\nx^2\n$$\n\n<div>html</div>\n\n\
                   [ref]: https://example.com\n\nInline $m + n$ math and <b>tags</b>.\n";
        assert_eq!(stats(src), s(6, 31, 26, 2, 2));
    }

    #[test]
    fn cjk_ideographs_are_one_word_each() {
        assert_eq!(stats("你好世界"), s(4, 4, 4, 1, 1));
        assert_eq!(stats("Hello 世界!"), s(3, 9, 8, 1, 1));
        assert_eq!(stats("日本語のテキスト"), s(5, 8, 8, 1, 1));
    }

    #[test]
    fn emoji_are_graphemes_but_not_words() {
        let src = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} family \u{1F44D}\u{1F3FD}";
        assert_eq!(stats(src), s(1, 10, 8, 1, 1));
        assert_eq!(stats("\u{e9} e\u{301}"), s(2, 3, 2, 1, 1));
    }

    #[test]
    fn blocks_are_counted_the_way_the_spec_describes() {
        let src = "# Title\n\nPara one.\n\n- item a\n- item b\n\n> quote\n\n```\ncode here\n```\n\n\
                   | a | b |\n|---|---|\n| c | d |\n";
        // Paragraphs: heading, paragraph, two list items, quote.
        assert_eq!(stats(src), s(14, 44, 40, 5, 5));
    }

    #[test]
    fn callout_title_and_body() {
        assert_eq!(
            stats("> [!NOTE] Heads up\n> body text\n"),
            s(4, 17, 15, 2, 2)
        );
    }

    #[test]
    fn tables_count_words_but_no_paragraphs() {
        assert_eq!(
            stats("| a b | c |\n|---|---|\n| d | e f |\n"),
            s(6, 8, 6, 0, 0)
        );
    }

    #[test]
    fn code_span_and_block_text_counts() {
        assert_eq!(stats("use `foo bar` now"), s(4, 15, 12, 1, 1));
        assert_eq!(stats("```\none two\nthree\n```\n"), s(3, 13, 11, 0, 0));
        assert_eq!(stats("```\n```\n"), Stats::default());
    }

    #[test]
    fn breaks_are_one_space() {
        assert_eq!(stats("one\ntwo"), s(2, 7, 6, 1, 1));
        assert_eq!(stats("one  \ntwo"), s(2, 7, 6, 1, 1));
        assert_eq!(stats("one\\\ntwo"), s(2, 7, 6, 1, 1));
    }

    #[test]
    fn wikilinks_escapes_entities_and_footnotes() {
        assert_eq!(
            stats("[[Note|shown text]] and [[Other#Head]]"),
            s(4, 20, 17, 1, 1)
        );
        assert_eq!(stats("[[#Heading]] [[Plain]]"), s(2, 13, 12, 1, 1));
        assert_eq!(stats("a\\*b"), s(2, 3, 3, 1, 1));
        assert_eq!(stats("fish &amp; chips"), s(2, 12, 10, 1, 1));
        assert_eq!(stats("text[^1]\n\n[^1]: note body\n"), s(3, 13, 12, 2, 2));
    }

    #[test]
    fn image_only_paragraph_is_not_a_text_paragraph() {
        assert_eq!(stats("![](pic.png)"), Stats::default());
    }

    #[test]
    fn task_items_count_their_text() {
        assert_eq!(stats("- [x] done thing\n- [ ] todo\n"), s(3, 14, 13, 2, 2));
    }

    #[test]
    fn selection_counts_only_the_selected_text() {
        let src = "Hello **bold** world\n\nSecond para here.";
        let sel = selected(src, "llo", "Second");
        assert_eq!(sel, s(4, 20, 18, 2, 2));
        assert_eq!(selected(src, "bold", "bold"), s(1, 4, 4, 1, 1));
        // Only the selected part of a text run counts.
        assert_eq!(selected(src, "para", "para"), s(1, 4, 4, 1, 1));
        // Markers inside the selection add nothing.
        assert_eq!(selected(src, "**bold**", "**bold**"), s(1, 4, 4, 1, 1));
    }

    #[test]
    fn selection_takes_atoms_whole_and_skips_outside_blocks() {
        let src = "a &amp; b `code span` c\n\nsecond";
        let amp = src.find("&amp;").unwrap();
        // One byte inside the entity selects the whole entity.
        let one = for_range(src, &parse_document(src), TextRange::new(amp + 1, amp + 2));
        assert_eq!(one, s(0, 1, 1, 1, 0));
        let code = src.find("code").unwrap();
        let two = for_range(src, &parse_document(src), TextRange::new(code, code + 1));
        assert_eq!(two, s(2, 9, 8, 1, 1));
        // Selection in the second block ignores the first one.
        let sec = src.find("second").unwrap();
        let three = for_range(src, &parse_document(src), TextRange::new(sec, src.len()));
        assert_eq!(three, s(1, 6, 6, 1, 1));
    }

    #[test]
    fn selection_in_code_blocks_and_tables() {
        let src = "```\nalpha beta\ngamma\n```\n\n| x y | z |\n|---|---|\n| w | v |\n";
        let doc = parse_document(src);
        let beta = src.find("beta").unwrap();
        let gamma_end = src.find("gamma").unwrap() + 3;
        // `beta`, the line break and `gam`.
        let code = for_range(src, &doc, TextRange::new(beta, gamma_end));
        assert_eq!(code, s(2, 8, 7, 0, 0));
        let z = src.find("z |").unwrap();
        let w = src.find("w |").unwrap();
        let table = for_range(src, &doc, TextRange::new(z, w + 1));
        assert_eq!(table, s(2, 2, 2, 0, 0));
    }

    #[test]
    fn selection_edge_cases() {
        let src = "日本語 text";
        let doc = parse_document(src);
        // Empty selection: nothing is selected.
        assert_eq!(for_range(src, &doc, TextRange::at(3)), Stats::default());
        // Offsets inside a character widen to the whole character.
        assert_eq!(for_range(src, &doc, TextRange::new(1, 4)), s(2, 2, 2, 1, 1));
        // Past the end clamps.
        assert_eq!(
            for_range(src, &doc, TextRange::new(10, 99)),
            s(1, 4, 4, 1, 1)
        );
        assert_eq!(
            for_range(src, &doc, TextRange::new(50, 99)),
            Stats::default()
        );
        // The whole text equals the document statistics.
        assert_eq!(
            for_range(src, &doc, TextRange::new(0, src.len())),
            compute(src, &doc)
        );
    }

    #[test]
    fn plain_text_counts_raw_text() {
        assert_eq!(compute_plain(""), Stats::default());
        assert_eq!(compute_plain("   \n\t\n\n"), Stats::default());
        assert_eq!(compute_plain("Hello world"), s(2, 11, 10, 1, 1));
        assert_eq!(compute_plain("a\tb"), s(2, 3, 2, 1, 1));
        assert_eq!(compute_plain("你好世界"), s(4, 4, 4, 1, 1));
        // Nothing is syntax: markers count as the characters they are.
        assert_eq!(compute_plain("**bold** and _italic_"), s(3, 21, 19, 1, 1));
        assert_eq!(compute_plain("# not a heading"), s(3, 15, 12, 1, 1));
    }

    #[test]
    fn plain_paragraphs_are_runs_of_non_blank_lines() {
        assert_eq!(
            compute_plain("Hello world.\n\nSecond one here.\n"),
            s(5, 28, 25, 2, 2)
        );
        // A line break inside a paragraph is one space and ends no sentence; blank lines are
        // not counted.
        assert_eq!(compute_plain("one\ntwo\n\n\n  \nthree"), s(3, 12, 11, 2, 2));
        // Indentation and trailing spaces are part of the paragraph.
        assert_eq!(compute_plain("  a  \n b"), s(2, 8, 2, 1, 1));
        assert_eq!(
            compute_plain("\n\nlast line without break"),
            s(4, 23, 20, 1, 1)
        );
        assert_eq!(plain_paragraphs("a\nb\n\nc\n\n\nd"), ["a\nb", "c", "d"]);
        assert!(plain_paragraphs("\n \n").is_empty());
    }

    #[test]
    fn plain_selection_counts_the_selected_text() {
        let src = "Hello world.\n\nSecond one here.";
        let start = src.find("world").unwrap();
        let end = src.find("Second").unwrap() + 6;
        assert_eq!(
            for_range_plain(src, TextRange::new(start, end)),
            s(2, 12, 12, 2, 2)
        );
        // Empty selection and offsets inside a character.
        let cjk = "日本語 text";
        assert_eq!(for_range_plain(cjk, TextRange::at(3)), Stats::default());
        assert_eq!(for_range_plain(cjk, TextRange::new(1, 4)), s(2, 2, 2, 1, 1));
        // Past the end clamps; the whole text equals the document statistics.
        assert_eq!(
            for_range_plain(cjk, TextRange::new(10, 99)),
            s(1, 4, 4, 1, 1)
        );
        assert_eq!(
            for_range_plain(cjk, TextRange::new(50, 99)),
            Stats::default()
        );
        assert_eq!(
            for_range_plain(src, TextRange::new(0, src.len())),
            compute_plain(src)
        );
    }

    #[test]
    fn reading_time_rounds_up_with_a_one_minute_floor() {
        let at = |words, wpm| {
            Stats {
                words,
                ..Stats::default()
            }
            .reading_minutes(wpm)
        };
        assert_eq!(at(0, 200), 0);
        assert_eq!(at(1, 200), 1);
        assert_eq!(at(200, 200), 1);
        assert_eq!(at(201, 200), 2);
        assert_eq!(at(400, 200), 2);
        assert_eq!(at(1000, 250), 4);
        assert_eq!(at(5, 0), 5);
        assert_eq!(at(usize::MAX, 1), u32::MAX);
    }
}
