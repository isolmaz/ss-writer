# Test fixture sources

Real third-party documents used as importer test inputs. None were generated for this project. All are redistributable under the licenses below.

## Upstream projects

| Project | License | License link |
|---|---|---|
| Apache POI (`test-data/document`), commit `ae62bb51…` | Apache-2.0 | https://github.com/apache/poi/blob/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/LICENSE |
| mammoth.js (`test/test-data`), commit `791a1892…` | BSD-2-Clause | https://github.com/mwilliamson/mammoth.js/blob/791a189230977143cde64dbf9f1d24dce96ac8a1/LICENSE |
| Apache Tika (`test-documents`), commit `49e40e6d…` | Apache-2.0 | https://github.com/apache/tika/blob/49e40e6ddaf44910111f6b8c44db1f768d39098b/LICENSE.txt |

## docx/

| file | commit-pinned URL | license | exercises |
|---|---|---|---|
| docx/mammoth_comments.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/comments.docx | BSD-2-Clause | comments |
| docx/mammoth_empty.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/empty.docx | BSD-2-Clause | minimal/empty document |
| docx/mammoth_endnotes.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/endnotes.docx | BSD-2-Clause | endnotes |
| docx/mammoth_footnote-hyperlink.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/footnote-hyperlink.docx | BSD-2-Clause | hyperlink inside footnote |
| docx/mammoth_footnotes.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/footnotes.docx | BSD-2-Clause | footnotes |
| docx/mammoth_simple-list.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/simple-list.docx | BSD-2-Clause | bulleted/numbered list |
| docx/mammoth_strict-format.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/strict-format.docx | BSD-2-Clause | ISO strict OOXML namespaces |
| docx/mammoth_strikethrough.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/strikethrough.docx | BSD-2-Clause | strikethrough run formatting |
| docx/mammoth_tables.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/tables.docx | BSD-2-Clause | tables |
| docx/mammoth_text-box.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/text-box.docx | BSD-2-Clause | text box / DrawingML shape |
| docx/mammoth_tiny-picture.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/tiny-picture.docx | BSD-2-Clause | embedded image |
| docx/mammoth_utf8-bom.docx | https://github.com/mwilliamson/mammoth.js/raw/791a189230977143cde64dbf9f1d24dce96ac8a1/test/test-data/utf8-bom.docx | BSD-2-Clause | UTF-8 BOM in XML parts (edge) |
| docx/poi_58067.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/58067.docx | Apache-2.0 | tracked changes (ins/del), Heading1 |
| docx/poi_61470.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/61470.docx | Apache-2.0 | non-Latin text (phonetic/Asian) |
| docx/poi_61787.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/61787.docx | Apache-2.0 | numbered lists, Heading1 |
| docx/poi_Bug54849.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/Bug54849.docx | Apache-2.0 | footnote + endnote, headers/footers |
| docx/poi_HeaderFooterUnicode.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/HeaderFooterUnicode.docx | Apache-2.0 | headers/footers with Unicode, page break, Heading1 |
| docx/poi_bug56075-changeTracking_on.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/bug56075-changeTracking_on.docx | Apache-2.0 | track-changes setting enabled |
| docx/poi_bug57031.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/bug57031.docx | Apache-2.0 | merged cells (gridSpan+vMerge), images, headers, page break |
| docx/poi_bug65649.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/bug65649.docx | Apache-2.0 | footnote, hyperlink, merged cells, lists, page break, images, non-Latin text |
| docx/poi_delins.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/delins.docx | Apache-2.0 | tracked insert/delete, hyperlinks, numbering |
| docx/poi_heading123.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/heading123.docx | Apache-2.0 | Heading1-3 hierarchy |
| docx/poi_rtl.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/rtl.docx | Apache-2.0 | right-to-left / non-Latin text, hyperlink |
| docx/poi_testComment.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/testComment.docx | Apache-2.0 | comments |
| docx/poi_truncated62886.docx | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/truncated62886.docx | Apache-2.0 | truncated ZIP (corrupt edge file) |
| docx/tika_testWORD_2006ml.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_2006ml.docx | Apache-2.0 | OMML equations, tracked changes, comments, footnotes/endnotes, hyperlinks, lists, page break, images, headers |
| docx/tika_testWORD_endnote_table.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_endnote_table.docx | Apache-2.0 | endnote referenced from a table |
| docx/tika_testWORD_features.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_features.docx | Apache-2.0 | tracked changes, comments |
| docx/tika_testWORD_numbered_list.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_numbered_list.docx | Apache-2.0 | multi-level numbered list, footnote, page break, image |
| docx/tika_testWORD_phonetic.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_phonetic.docx | Apache-2.0 | East-Asian phonetic guide (ruby) / non-Latin text |
| docx/tika_testWORD_protected_passtika.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_protected_passtika.docx | Apache-2.0 | password-encrypted OOXML (must be rejected gracefully) |
| docx/tika_testWORD_truncated.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_truncated.docx | Apache-2.0 | truncated/corrupt docx (edge) |
| docx/tika_testWORD_various.docx | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testWORD_various.docx | Apache-2.0 | footnote, hyperlink, list, image, non-Latin text, header |

## doc/

| file | commit-pinned URL | license | exercises |
|---|---|---|---|
| doc/DiffFirstPageHeadFoot.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/DiffFirstPageHeadFoot.doc | Apache-2.0 | different first-page header/footer |
| doc/Fuzzed.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/Fuzzed.doc | Apache-2.0 | corrupted/fuzzed edge file |
| doc/HeaderFooterUnicode.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/HeaderFooterUnicode.doc | Apache-2.0 | headers/footers, non-Latin text |
| doc/Lists.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/Lists.doc | Apache-2.0 | numbered/bulleted lists |
| doc/PasswordProtected.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/PasswordProtected.doc | Apache-2.0 | encrypted file (must be rejected gracefully) |
| doc/Word6.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/Word6.doc | Apache-2.0 | legacy Word 6 format |
| doc/Word95.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/Word95.doc | Apache-2.0 | legacy Word 95 format |
| doc/empty.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/empty.doc | Apache-2.0 | empty document |
| doc/endingnote.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/endingnote.doc | Apache-2.0 | endnotes |
| doc/equation.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/equation.doc | Apache-2.0 | equation object |
| doc/footnote.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/footnote.doc | Apache-2.0 | footnotes |
| doc/header_image.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/header_image.doc | Apache-2.0 | header with image |
| doc/hyperlink.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/hyperlink.doc | Apache-2.0 | hyperlink field |
| doc/innertable.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/innertable.doc | Apache-2.0 | nested table |
| doc/page-break.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/page-break.doc | Apache-2.0 | page breaks |
| doc/picture.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/picture.doc | Apache-2.0 | embedded picture (1.4 MB) |
| doc/simple-list.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/simple-list.doc | Apache-2.0 | simple list |
| doc/simple-table.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/simple-table.doc | Apache-2.0 | simple table |
| doc/table-merges.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/table-merges.doc | Apache-2.0 | merged table cells |
| doc/test-fields.doc | https://github.com/apache/poi/raw/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/test-data/document/test-fields.doc | Apache-2.0 | fields |

## rtf/

| file | commit-pinned URL | license | exercises |
|---|---|---|---|
| rtf/testComment.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testComment.rtf | Apache-2.0 | annotations (comments) |
| rtf/testRTF-ms932.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTF-ms932.rtf | Apache-2.0 | Shift-JIS (CP932) text |
| rtf/testRTFBoldItalic.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFBoldItalic.rtf | Apache-2.0 | bold/italic runs |
| rtf/testRTFCorruptListOverride.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFCorruptListOverride.rtf | Apache-2.0 | corrupt list override (edge) |
| rtf/testRTFHexEscapeInsideWord.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFHexEscapeInsideWord.rtf | Apache-2.0 | \'hh escape inside a word |
| rtf/testRTFHyperlink.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFHyperlink.rtf | Apache-2.0 | HYPERLINK fields |
| rtf/testRTFInvalidUnicode.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFInvalidUnicode.rtf | Apache-2.0 | invalid Unicode escape (edge) |
| rtf/testRTFJapanese.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFJapanese.rtf | Apache-2.0 | Japanese text, list table, table |
| rtf/testRTFListLibreOffice.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFListLibreOffice.rtf | Apache-2.0 | lists written by LibreOffice, Unicode escapes |
| rtf/testRTFListMicrosoftWord.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFListMicrosoftWord.rtf | Apache-2.0 | lists written by Word |
| rtf/testRTFRegularImages.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFRegularImages.rtf | Apache-2.0 | embedded \pict images (PNG/WMF) |
| rtf/testRTFTIKA_2899.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFTIKA_2899.rtf | Apache-2.0 | bookmarks, fields, tables, lists, old-style bullets |
| rtf/testRTFTableCellSeparation.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFTableCellSeparation.rtf | Apache-2.0 | table rows/cells |
| rtf/testRTFUnicodeGothic.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFUnicodeGothic.rtf | Apache-2.0 | supplementary-plane Unicode (Gothic) |
| rtf/testRTFVarious.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFVarious.rtf | Apache-2.0 | footnotes, header, list table, tables, fields, Unicode escapes |
| rtf/testRTFWindowsCodepage1250.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFWindowsCodepage1250.rtf | Apache-2.0 | Windows-1250 codepage text |
| rtf/testRTFWord2010CzechCharacters.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTFWord2010CzechCharacters.rtf | Apache-2.0 | Czech text, codepage + \u escapes |
| rtf/testRTF_annotation_spacing.rtf | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-microsoft-module/src/test/resources/test-documents/testRTF_annotation_spacing.rtf | Apache-2.0 | annotations (comments) |

## odt/

| file | commit-pinned URL | license | exercises |
|---|---|---|---|
| odt/LibreOfficeWriter_odt_1.3.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/versions/LibreOfficeWriter_odt_1.3.odt | Apache-2.0 | plain LibreOffice Writer 1.3 export |
| odt/testFooter.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testFooter.odt | Apache-2.0 | footer, soft page break |
| odt/testNPEOpenDocument.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testNPEOpenDocument.odt | Apache-2.0 | headings, lists, hyperlinks |
| odt/testODFwithOOo3.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testODFwithOOo3.odt | Apache-2.0 | footnotes, images, hyperlinks, lists, tracked changes, comments, page break, embedded object |
| odt/testODTEmbeddedImageLink.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testODTEmbeddedImageLink.odt | Apache-2.0 | linked/embedded image |
| odt/testODTEncrypted.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testODTEncrypted.odt | Apache-2.0 | encrypted ODT (must be rejected gracefully) |
| odt/testODTStyles2.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testODTStyles2.odt | Apache-2.0 | tables with merged cells, page break styles |
| odt/testODTStyles3.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testODTStyles3.odt | Apache-2.0 | tables with merged cells, comments, page break |
| odt/testODT_svgTitleInStyledSpan.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testODT_svgTitleInStyledSpan.odt | Apache-2.0 | table, non-Latin text, svg:title in styled span |
| odt/testODTnotaZipFile.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testODTnotaZipFile.odt | Apache-2.0 | 24-byte non-ZIP file with .odt extension (corrupt edge) |
| odt/testOpenOffice2.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testOpenOffice2.odt | Apache-2.0 | OpenOffice 2 document |
| odt/testStyles.odt | https://github.com/apache/tika/raw/49e40e6ddaf44910111f6b8c44db1f768d39098b/tika-parsers/tika-parsers-standard/tika-parsers-standard-modules/tika-parser-miscoffice-module/src/test/resources/test-documents/testStyles.odt | Apache-2.0 | lists, styles |

## Notices

- Files are unmodified copies of upstream test data; file names carry a `mammoth_`/`poi_`/`tika_` prefix for provenance in docx/ (otherwise upstream names are kept). The `doc/` files keep upstream names.
- Apache POI and Apache Tika: Licensed under the Apache License, Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0). Copyright The Apache Software Foundation. See each project's NOTICE file at the pinned commit: https://github.com/apache/poi/blob/ae62bb5116b9aee19ebd5834e3a82066132c9f7f/NOTICE and https://github.com/apache/tika/blob/49e40e6ddaf44910111f6b8c44db1f768d39098b/NOTICE.txt
- mammoth.js: Copyright (c) 2013, Michael Williamson. BSD-2-Clause, text at the license link above.
- Intentionally malformed/encrypted files (`poi_truncated62886.docx`, `tika_testWORD_truncated.docx`, `tika_testWORD_protected_passtika.docx`, `doc/Fuzzed.doc`, `doc/PasswordProtected.doc`, `rtf/testRTFCorruptListOverride.rtf`, `rtf/testRTFInvalidUnicode.rtf`, `odt/testODTEncrypted.odt`, `odt/testODTnotaZipFile.odt`) are edge-case inputs; importers must fail gracefully, not crash.

---

# SOURCES-text (FixB)

| file | commit-pinned URL | license | exercises |
|---|---|---|---|
| mermaid/flowchart-101.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/flowchart.md (code block extracted verbatim) | MIT | flowchart LR/TB, subgraph, node shapes, links |
| mermaid/flowchart-08.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/flowchart.md (code block extracted verbatim) | MIT | flowchart LR/TB, subgraph, node shapes, links |
| mermaid/sequence-33.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/sequenceDiagram.md (code block extracted verbatim) | MIT | sequenceDiagram participants, messages, notes |
| mermaid/sequence-03.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/sequenceDiagram.md (code block extracted verbatim) | MIT | sequenceDiagram participants, messages, notes |
| mermaid/class-01.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/classDiagram.md (code block extracted verbatim) | MIT | classDiagram members, relations |
| mermaid/class-22.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/classDiagram.md (code block extracted verbatim) | MIT | classDiagram members, relations |
| mermaid/state-20.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/stateDiagram.md (code block extracted verbatim) | MIT | stateDiagram-v2 composite/transitions |
| mermaid/state-17.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/stateDiagram.md (code block extracted verbatim) | MIT | stateDiagram-v2 composite/transitions |
| mermaid/er-12.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/entityRelationshipDiagram.md (code block extracted verbatim) | MIT | erDiagram entities, relations, attributes |
| mermaid/er-15.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/entityRelationshipDiagram.md (code block extracted verbatim) | MIT | erDiagram entities, relations, attributes |
| mermaid/gantt-11.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/gantt.md (code block extracted verbatim) | MIT | gantt sections, date tasks, dependencies |
| mermaid/gantt-02.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/gantt.md (code block extracted verbatim) | MIT | gantt sections, date tasks, dependencies |
| mermaid/pie-02.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/pie.md (code block extracted verbatim) | MIT | pie title/data |
| mermaid/pie-01.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/pie.md (code block extracted verbatim) | MIT | pie title/data |
| mermaid/mindmap-11.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/mindmap.md (code block extracted verbatim) | MIT | mindmap indentation tree, shapes |
| mermaid/mindmap-01.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/mindmap.md (code block extracted verbatim) | MIT | mindmap indentation tree, shapes |
| mermaid/timeline-04.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/timeline.md (code block extracted verbatim) | MIT | timeline sections/events |
| mermaid/timeline-09.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/timeline.md (code block extracted verbatim) | MIT | timeline sections/events |
| mermaid/gitgraph-14.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/gitgraph.md (code block extracted verbatim) | MIT | gitGraph commit/branch/merge |
| mermaid/gitgraph-11.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/gitgraph.md (code block extracted verbatim) | MIT | gitGraph commit/branch/merge |
| mermaid/journey-01.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/userJourney.md (code block extracted verbatim) | MIT | journey sections/tasks/actors |
| mermaid/quadrant-03.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/quadrantChart.md (code block extracted verbatim) | MIT | quadrantChart axes/points |
| mermaid/quadrant-01.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/quadrantChart.md (code block extracted verbatim) | MIT | quadrantChart axes/points |
| mermaid/xychart-08.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/xyChart.md (code block extracted verbatim) | MIT | xychart-beta bar/line series |
| mermaid/xychart-03.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/xyChart.md (code block extracted verbatim) | MIT | xychart-beta bar/line series |
| mermaid/sankey-01.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/sankey.md (code block extracted verbatim) | MIT | sankey-beta CSV flows |
| mermaid/sankey-08.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/sankey.md (code block extracted verbatim) | MIT | sankey-beta CSV flows |
| mermaid/block-27.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/block.md (code block extracted verbatim) | MIT | block-beta columns/blocks |
| mermaid/block-26.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/block.md (code block extracted verbatim) | MIT | block-beta columns/blocks |
| mermaid/requirement-05.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/requirementDiagram.md (code block extracted verbatim) | MIT | requirementDiagram requirement/element/relationship |
| mermaid/requirement-03.mmd | https://github.com/mermaid-js/mermaid/blob/33d0a4997b742df0d04838cce3f572e674d1cf9d/packages/mermaid/src/docs/syntax/requirementDiagram.md (code block extracted verbatim) | MIT | requirementDiagram requirement/element/relationship |
| text/uchardet-tr-iso-8859-9.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/tr/iso-8859-9.txt | MPL-1.1 (uchardet COPYING) | Turkish ISO-8859-9 (no BOM) detection/decoding |
| text/uchardet-tr-utf-8.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/tr/utf-8.txt | MPL-1.1 (uchardet COPYING) | Turkish UTF-8 without BOM |
| text/uchardet-ja-shift_jis.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/shift_jis.txt | MPL-1.1 (uchardet COPYING) | Shift-JIS (short, ~115 B) |
| text/uchardet-ja-utf-16be.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/utf-16be.txt | MPL-1.1 (uchardet COPYING) | UTF-16BE WITHOUT BOM |
| text/uchardet-ja-utf-16le.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/utf-16le.txt | MPL-1.1 (uchardet COPYING) | UTF-16LE WITHOUT BOM |
| text/uchardet-ja-utf-8.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/utf-8.txt | MPL-1.1 (uchardet COPYING) | Japanese UTF-8 no BOM |
| text/uchardet-zh-gb18030.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/zh/gb18030.txt | MPL-1.1 (uchardet COPYING) | GB18030 (short, 88 B) |
| text/uchardet-zh-utf-8.txt | https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/zh/utf-8.txt | MPL-1.1 (uchardet COPYING) | Chinese UTF-8 no BOM |
| text/derived-tr-windows-1254.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/tr/utf-8.txt | MPL-1.1 (uchardet COPYING) | windows-1254 (cp1254) Turkish; transcoded by FixB from the UTF-8 file |
| text/derived-tr-utf-16le-bom.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/tr/utf-8.txt | MPL-1.1 (uchardet COPYING) | UTF-16LE with BOM FF FE; transcoded from UTF-8 file |
| text/derived-tr-utf-16be-bom.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/tr/utf-8.txt | MPL-1.1 (uchardet COPYING) | UTF-16BE with BOM FE FF; transcoded from UTF-8 file |
| text/derived-tr-utf-8-bom.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/tr/utf-8.txt | MPL-1.1 (uchardet COPYING) | UTF-8 with BOM EF BB BF; BOM prepended to UTF-8 file |
| text/derived-ja-utf-16le-bom.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/utf-16le.txt | MPL-1.1 (uchardet COPYING) | UTF-16LE with BOM; BOM prepended to upstream file |
| text/derived-ja-utf-16be-bom.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/utf-16be.txt | MPL-1.1 (uchardet COPYING) | UTF-16BE with BOM; BOM prepended to upstream file |
| text/derived-ja-shift_jis.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/utf-8.txt | MPL-1.1 (uchardet COPYING) | Shift-JIS longer sample (~600 B); transcoded from UTF-8 file |
| text/derived-ja-gb18030.txt | derived from https://gitlab.freedesktop.org/uchardet/uchardet/-/raw/06029ec3340cdf6bf9a6a537dafb3f39eda0560e/test/ja/utf-8.txt | MPL-1.1 (uchardet COPYING) | GB18030 encoding of Japanese text (kana/kanji in 2-byte & 4-byte forms); transcoded |
| html/lexical-gdocs-fontsize-pt.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/packages/lexical-playground/__tests__/e2e/CopyAndPaste/lexical/CopyAndPaste.spec.mjs#L861 (text/html clipboard string extracted verbatim from test) | MIT | Google Docs clipboard with pt font-size spans |
| html/lexical-gdocs-fontsize-px-pt.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/packages/lexical-playground/__tests__/e2e/CopyAndPaste/lexical/CopyAndPaste.spec.mjs#L885 (text/html clipboard string extracted verbatim from test) | MIT | Google Docs clipboard with mixed px/pt font-size, multiple paragraphs |
| html/lexical-gdocs-textformat.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/packages/lexical-playground/src/__tests__/browser/TextFormatHTMLCopyAndPaste.test.ts#L22 (text/html clipboard string extracted verbatim from test) | MIT | Google Docs clipboard: docs-internal-guid <b> wrapper, styled spans (bold/italic/underline/strike mixes) |
| html/lexical-gdocs-table.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/packages/lexical-playground/src/__tests__/browser/TablesHTMLCopyAndPaste.test.ts#L22 (text/html clipboard string extracted verbatim from test) | MIT | Google Docs clipboard table with custom widths, nested p/span styles |
| html/lexical-gsheets-table.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/packages/lexical-playground/src/__tests__/browser/TablesHTMLCopyAndPaste.test.ts#L160 (text/html clipboard string extracted verbatim from test) | MIT | Google Sheets clipboard: <google-sheets-html-origin>, table, mso-data-placement |
| html/lexical-word.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/dev-examples/dom-import/src/fixtures/word.html | MIT | Microsoft Word (Mac, Word 15) clipboard HTML: mso-list MsoListParagraph*, o:p, conditional comments, xml namespaces |
| html/lexical-vscode-safari.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/dev-examples/dom-import/src/fixtures/vscode-safari.html | MIT | VS Code code copied via Safari: styled <div> per line, white-space:pre, inline spans for syntax highlight |
| html/lexical-browser-table.html | https://github.com/facebook/lexical/blob/b36ab04d4e813d214338692063687e69522b1963/packages/lexical-playground/src/__tests__/browser/TablesHTMLCopyAndPaste.test.ts#L84 (text/html string extracted verbatim) | MIT | Chrome-style browser clipboard: <meta charset='utf-8'> prefix, table with col widths and inline border styles |
