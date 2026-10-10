# Golden Markdown for tests/fixtures/odt

Derived from the ODF `content.xml` / `styles.xml` of the Apache Tika test documents (Apache-2.0, see tests/fixtures/SOURCES.md).
The expected output follows the ODF semantics, not the importer's code: style-derived emphasis (`fo:font-weight`, `fo:font-style`,
`style:text-underline-style`), collapsed white space, `text:s` / `text:tab` / `text:line-break`, list styles (numbered vs bullet),
the first table row as header row, covered cells as empty cells, annotations and tracked changes dropped, footnotes as `[^n]`
definitions, linked images extracted to `<stem>_assets`.

| Golden | Fixture | What it pins down |
| --- | --- | --- |
| `LibreOfficeWriter_odt_1.3.md` | `LibreOfficeWriter_odt_1.3.odt` | one plain paragraph |
| `testOpenOffice2.md` | `testOpenOffice2.odt` | `text:s` between words, empty paragraph |
| `testStyles.md` | `testStyles.odt` | italic/bold/underline spans, numbered list then bullet list |
| `testODTEmbeddedImageLink.md` | `testODTEmbeddedImageLink.odt` | image inside a link, width and alt text |
| `testFooter.md` | `testFooter.odt` | soft page break, empty paragraphs, footer loss |
| `testODTStyles2.md` | `testODTStyles2.odt` | merged cells, bold paragraph styles, tabs |
| `testODTStyles3.md` | `testODTStyles3.odt` | merged cells, multi-paragraph cell, annotations inside a paragraph |
| `testNPEOpenDocument.md` | `testNPEOpenDocument.odt` | heading link, numbered/bullet lists, nested quote paragraphs in list items |
| `testODT_svgTitleInStyledSpan.md` | `testODT_svgTitleInStyledSpan.odt` | East Asian bold run ending in `—` before a letter (the dash moves outside the strong markers), centered table column (`:-:`), shapes, footer loss |
| `testODFwithOOo3.md` | `testODFwithOOo3.odt` | heading with comment, footnote, tracked changes, shape text, MathML objects, page break, bibliography |
