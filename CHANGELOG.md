# Changelog

All notable changes to SS Writer are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Every file whose content is text opens, whatever it is called: `Dockerfile`, `.gitignore`, `.kt`, a file without an extension. A file is text when it starts with a byte order mark or holds no `0x00` byte in its first 8192 bytes; a binary file is refused with "This file is not a text file" instead of being shown as mojibake. Clicking such a file in the sidebar, Quick Open or a folder search still hands it to its default app.
- The installer registers SS Writer as an application (`Applications\SSWriter.exe`) and lists it under `*\OpenWithProgids`, so Windows offers it in "Open with" for any file, with `.json`, `.csv`, `.log`, `.xml`, `.html`, `.css`, `.js`, `.yml` and `.py` naming it directly. No default association is taken.

### Fixed

- Exporting a plain-text document read its text as Markdown, so a `.json` or `.html` file lost its `#`, `*` and `|` characters to headings, emphasis and tables in PDF, HTML, DOCX, ODT, RTF and ePub. A plain document is now exported verbatim: as its own text for TXT, Markdown with attachments and TextBundle, inside a code block for every other format.
- The table formatter re-aligned pipe characters in plain-text and Mermaid documents when the caret left the line. It now runs only in Markdown documents.

## [0.1.5] - 2026-10-13

### Fixed

- Unsaved text of an untitled document was lost when SS Writer was closed: the session remembered the tab, but the text was written after the windows had been closed, and a closed window drops its documents, so the next start found a tab without a buffer and dropped it. The text is now stored while the tabs are still open.
- The update left the copy of the application it installed from (about 60 MB) in the temp folder, because the updater cannot delete itself while it runs. The next start removes it.

## [0.1.4] - 2026-10-13

### Fixed

- The mouse pointer kept the shape of the surface it came from until it moved again. Windows asks a window which cursor to show before it delivers the move that decides it, so the shape chosen while handling that move only reached the screen with the next move: a pointer that jumps (a shortcut that opens a window below it, a window that opens under a resting pointer, remote control) showed the wrong shape until it was moved.

## [0.1.3] - 2026-10-13

### Added

- The update banner downloads the new version itself: Download fetches the release's installer while a bar in the banner shows how far it got, checks it against the release's `SHA256SUMS.txt`, and then offers "Restart and install", which closes SS Writer, installs the package and opens the new version. A package that does not match its checksum is deleted and the reason is shown. A portable copy still gets the release page.
- Tooltips: the buttons that are only an icon name themselves after the pointer rests on them — the selection toolbar, the B/I/U pill, the sidebar and inspector toggles, the sidebar header buttons, the find bar and the Encoding and Line-ending segments of the status bar.

### Fixed

- The mouse pointer kept the text cursor everywhere outside the text: over the title bar, tabs, the sidebar, the status bar, banners, buttons and dialogs. Every surface now asks for its own shape: a hand over anything clickable, the text cursor over text and text boxes, the arrow over everything else. In the editor the image and diagram bodies, dividers, a narrow table's margin and the empty area below the last block are no longer text either.
- B, I and U of the pill and the buttons of the selection toolbar did nothing: clicking them moved the keyboard focus out of the editor. The editor now keeps the focus and the selection, so the buttons do what their shortcuts do.
- In a plain-text document (`.txt`) the formatting buttons wrote nothing because Markdown markers do not belong there, but the UI still offered them: the pill is now disabled and the selection toolbar does not appear. "Render as Markdown" turns both on again.
- Clicking a control in the Settings window (and in the export dialog and the history panel) did not give it the keyboard, so the editor font, font size and line height could not be typed and the shortcut filter stayed empty unless the control was reached with Tab. A click now focuses the control, and the settings reach every open tab of every window at once.
- An open drop-down in the Settings window and in the export dialog was painted behind the rows below it; open lists are now drawn above everything else.

### Removed

- A second, unused B/I/U pill implementation (`FormatPill` in the popups module) and the format-state scanning that only served it; the pill the window shows is the title bar's.

## [0.1.2] - 2026-10-12

### Fixed

- A test of the animation-frame idle sleep compared it against the refresh interval read a second time from DWM. DWM measures the rate, so the second reading differed by a fraction of a microsecond under load and the test failed at random. It now compares against the interval the application itself sleeps for (`UiApp::frame_interval`).

### Removed

- The test that opened exported DOCX and ODT files in a headless LibreOffice (`crates/ssw-convert/tests/office_libreoffice.rs`): it blocked the whole test run for minutes on machines where LibreOffice was installed. Nothing in the repository starts an office suite any more. What the exporters write stays covered by the package, schema-order and write → read round-trip tests, and opening the result in Word is a manual checklist item.

## [0.1.1] - 2026-10-12

### Fixed

- Starting SS Writer no longer opens an empty console window next to the editor.
- Underline on struck text (and strikethrough on underlined text) produced three tildes, which Markdown shows as plain tildes; the second command then kept adding tildes. The two styles now replace each other, as `~x~` and `~~x~~` cannot nest.
- Shift+Tab on a bullet nested in a numbered list (or a numbered item nested in a bullet list) left a marker of the wrong kind that split the list in three; the item now takes the marker of the list it joins. Tab in a list item that cannot be indented inserted a tab character into its text.
- Format → Quote with the caret in a callout removed the `>` of that line alone and left a broken callout; it now removes the whole callout (a title stays as a line of text).
- Typing in a large document no longer parses the whole text on every keystroke: the editor now uses the incremental parser, so a keystroke in a 60 000-line document costs milliseconds instead of a quarter of a second.
- The `[[` completion list shows the headings (`[[#`) and the workspace files as soon as they arrive instead of staying empty until the next keystroke.
- Edit → Insert date (F5), Format → Image and Format → Table did nothing. F5 inserts the long date of the Windows locale (escaped when it would start a numbered list; plain text files take it as it is), Image asks for picture files and inserts them like pasted files, Table inserts an empty two-column table with the caret in its first cell.
- The Copy button of a code block put nothing on the clipboard, clicking a footnote reference did not jump to its definition (also when the two spell the label in different case), and text dragged onto another tab of the window was dropped on the floor instead of moving (Ctrl: copying) to that tab; a read-only tab such as a Help page takes nothing, so the dragged text stays where it is.
- The sidebar commands (open, rename, duplicate, delete, reveal, copy path, new file and folder, collapse and expand all) were listed in the command palette and assignable to shortcuts but ran only from the sidebar's context menu; from the palette or a shortcut they now act on the selected entry (or the workspace). They are disabled without a workspace; the entry commands also without a selected entry or while the sidebar is hidden, and the open commands for a selected folder.
- Deleting, cutting or joining items of a numbered list (and typing a marker into one) renumbers the list, as Enter already did.
- Heading folding did nothing: the gutter chevron, View → Fold all / Unfold all, unfolding of search hits and the per-file fold state were not connected to the editor.
- The caret position, scroll position and folds of the tabs that were open when SS Writer closed were not remembered; restored tabs and reopened files started at the top. The scroll position was never restored at all.
- Ctrl+click follows links to Markdown, text and word-processor files and `mailto:` links, and a `#heading` suffix no longer stops a `.md` link from opening. Images and other files in the document's folder or below open with their default app; executable content (also behind a trailing dot, as in `setup.exe.`), files outside that folder (absolute paths, network shares, `..`) and links with other URI schemes (`file:`, `javascript:`) are ignored.

## [0.1.0] - 2026-10-10

First release.

### Added

#### Plain text

- Plain text mode: View → Word wrap for text files, which wrap long lines at the window edge by default; switched off, every line stays on one row and the view scrolls sideways (Shift+wheel, a horizontal wheel, the caret, UI Automation). View → Render as Markdown shows a text file as live Markdown (headings in the inspector, Markdown paste and copy) without changing how it is saved: file name, encoding, BOM and line endings stay. Both items are checkable, enabled only for plain text files, listed in the command palette and have no default shortcut (assignable in Settings → Shortcuts). Word wrap belongs to the tab and Render as Markdown to the document; neither is stored between sessions.

#### Window and appearance

- Native Windows 11 window with a custom title bar that holds the tabs, a Mica Alt backdrop (solid color when transparency effects are off, on battery saver or when the window is inactive), Snap Layouts on the maximize button and per-monitor DPI.
- Light and dark themes that follow the system live, with accent color System, Blue or custom, optional Mica, and high contrast support.
- Native popup menus: File, Edit, Format, View, Window and Help.
- Window menu with New window, Merge all windows (moves the tabs of every other window into the current one) and the tab commands.
- Status bar with line and column, word count, zoom, encoding and line endings; the encoding and line-ending segments are clickable.
- Inspector with a Contents tab (collapsible heading tree with H1 to H6 level filter) and an Info tab (words, characters, paragraphs, sentences, reading time, file information and backlinks; selection-aware).

#### Editor

- Live Markdown rendering: markers hide until the caret touches the element, heading badges (H1 to H6) with a level menu, and an "Auto-hide Markdown" setting.
- Smart typing: lists and tasks continue and renumber, Tab and Shift+Tab indent items, fences and math blocks close themselves, a pasted URL over a selection becomes a link, tasks toggle with a click or Ctrl+., completed tasks move to the end of their list, and F5 inserts the date.
- Smart paste: images become files in the attachment folder, copied files become embeds or attachment chips, Word, Google Docs and browser HTML convert to Markdown, VS Code text stays plain, and Ctrl+Shift+V always pastes plain text.
- Selection and navigation: Unicode word boundaries, double- and triple-click, drag and drop (also onto other tabs), Ctrl+click opens a link, and full keyboard navigation.
- Operation-based undo and redo in which typing groups naturally and undo restores the selection.
- Spell check with the Windows spell checker: squiggles, suggestions, Add to dictionary and Ignore; code, URLs, math and front matter are skipped.
- Typewriter mode with a configurable position and dead zone, focus mode, full screen, zen mode and app-wide zoom.
- Heading folding from the gutter chevron; fold state is stored per file, never in the document.
- B I U formatting pill and a selection toolbar with headings, lists, highlight colors, links, tables, images and more.

#### Markdown elements

- Headings, emphasis, underline, strikethrough, highlights (plain and colored), inline code, bulleted, numbered and task lists, quotes, callouts (note, tip, important, warning, caution), fenced code blocks with a language label, a Copy button and optional syntax highlighting, dividers, front matter, page breaks, footnotes with hover preview, and raw HTML shown as dimmed text.
- Math (`$x$` and `$$…$$`) drawn as vectors through Typst and MiTeX, also inside tables.
- Mermaid diagrams in fenced `mermaid` blocks, drawn as vectors and exportable as PNG or SVG; diagram types the renderer cannot draw correctly fall back to the source with an "Unsupported diagram" note.
- Tables edited in place: Tab, Shift+Tab and Enter navigation, a cell context menu (rows, columns, alignment), Copy table as Markdown, CSV or HTML, Edit as raw Markdown, and automatic pipe alignment when the caret leaves the table.
- Links (inline, reference and autolinks) and wikilinks (`[[target]]`, aliases, headings) with fuzzy completion, creation of missing targets inside the workspace, link updates when a file is renamed or moved, and backlinks in the inspector.
- Images (PNG, JPEG, GIF, BMP, TIFF, WebP, ICO, HEIC when the codec is installed, and SVG) with resize handles; remote `https` images with a disk cache and safe defaults; attachment chips for other files; configurable attachment folders and naming template.

#### Files and formats

- Open and save Markdown (`.md`, `.markdown`, `.mdx`), plain text and other text files, Mermaid (`.mmd`), TextBundle (`.textbundle`, `.textpack`), DOCX, ODT and RTF; import legacy Word `.doc` files. Every import reports what could not be represented in a banner with a details list; imported documents can be saved back to their original format (except `.doc`).
- Document kinds by file type: live Markdown, plain text mode for text files (every line shown as written, nothing rendered) and Mermaid source with a live diagram for `.mmd` files.
- Encoding detection (BOM, UTF-16/32, UTF-8, legacy code pages) with encoding, BOM and line endings preserved on save; status bar menus to reopen a file with another encoding, to save with another encoding and to save with LF, CRLF or CR.
- Autosave (on by default, one second after typing stops), atomic writes with retries on sharing violations, read-only handling, external change detection with diff-based reload and a conflict banner (Keep mine, Load theirs, Compare), and OneDrive placeholders that indexing never hydrates.
- New and untitled documents: Ctrl+N in a workspace creates `Untitled.md` with inline rename; without a workspace the unsaved buffer stays in the session with a periodic backup. Opening a file replaces an Untitled tab that was never used instead of adding a new tab.
- Version history: deduplicated snapshots with a panel to list, diff and restore, kept for 30 days by default.
- Crash recovery: emergency save of unsaved documents, an offer to recover them at the next launch, a local rolling log and no telemetry.

#### Workspace, tabs and search

- Workspace sidebar: live file tree, sorting and filters, context menu (open, rename, duplicate, delete to the Recycle Bin, reveal in Explorer, copy path and more), drag and drop, and a background index that respects `.gitignore`.
- Tabs that reorder, move between windows and show a dirty dot when autosave is off, documents shared between tabs and windows, single-instance behavior, closed-tab reopening and session restore (windows, tabs, workspaces, caret, scroll and folds).
- Find and replace with Match case, Whole word and Regex options, replace all as one undo step, go to line, fuzzy Quick Open for files and headings, and search in folder.
- Command palette that lists every command with its shortcut; commands that do not apply to the active document are shown disabled and do not run.

#### Copy, export and print

- Copy as plain text, Markdown, rich text or HTML; copied Markdown pastes back exactly into SS Writer; Share through the system share sheet.
- Export to PDF, PNG, JPG, HTML (single file), ePub, DOCX, ODT, RTF, TXT, Markdown with attachments and TextBundle, with page size, margins, base font size, page numbers, black text, light theme and title options, and taskbar progress for long exports.
- Printing through the system print dialog.

#### Settings and shortcuts

- Settings window (Ctrl+,) with General, Appearance, Editor, Sidebar, Attachments, Export, Shortcuts and Advanced pages, backed by a human-editable `settings.json`.
- Default apps… button in Settings > General that opens the Windows page for choosing the app that opens Markdown and text files.
- Complete default keymap without Ctrl+Alt bindings, every command rebindable, conflicts flagged, and a Help > Keyboard shortcuts document.

#### Windows integration and distribution

- Per-user MSI installer (no administrator rights) that adds Open with entries for Markdown, Mermaid, text, TextBundle, DOCX, DOC, ODT and RTF files, an Open with SS Writer folder verb and an Explorer preview pane for `.md`, `.markdown` and `.mmd`.
- Portable zip with an empty `portable` file that keeps settings and data next to the executable.
- Winget package `isolmaz.SSWriter`.
- Weekly update check against the GitHub releases of this repository: notification only, and it can be turned off in Settings > General.
- Jump List with recent files and folders and New window; touch and pen input, DirectManipulation scrolling, text services (IME, dictation, emoji panel) and UI Automation for every control.

### Known issues

- Scrolling very large documents (60 000 lines) at high speed does not yet keep up with a 144 Hz display: in the stress test a frame takes about 16 ms instead of 7 ms. See [docs/performance.md](docs/performance.md#known-issues).

[Unreleased]: https://github.com/isolmaz/ss-writer/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/isolmaz/ss-writer/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/isolmaz/ss-writer/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/isolmaz/ss-writer/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/isolmaz/ss-writer/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/isolmaz/ss-writer/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/isolmaz/ss-writer/releases/tag/v0.1.0
