# Golden Markdown for tests/fixtures/doc

Expected outputs were produced by the hand-verified spike `ssw-spikes/g-doc` (see its REPORT.md: simple-list, simple-table,
innertable, table-merges, footnote, hyperlink hand-checked against the Apache POI test expectations; HeaderFooterUnicode matches
its .docx twin word for word, including the `# Molière` heading) and adjusted for the production importer:
page breaks are emitted as `<!-- pagebreak -->` (page-break, HeaderFooterUnicode). Input documents: Apache POI test-data
(Apache-2.0), see tests/fixtures/SOURCES.md. Twin .docx truth files remain in ssw-spikes/g-doc/truth (not copied).
