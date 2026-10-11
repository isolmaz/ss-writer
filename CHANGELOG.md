# Changelog

All notable changes to SS Writer are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Starting SS Writer no longer opens an empty console window next to the editor.
- Underline on struck text (and strikethrough on underlined text) produced three tildes, which Markdown shows as plain tildes; the second command then kept adding tildes. The two styles now replace each other, as `~x~` and `~~x~~` cannot nest.
- Shift+Tab on a bullet nested in a numbered list (or a numbered item nested in a bullet list) left a marker of the wrong kind that split the list in three; the item now takes the marker of the list it joins. Tab in a list item that cannot be indented inserted a tab character into its text.
- Format → Quote with the caret in a callout removed the `>` of that line alone and left a broken callout; it now removes the whole callout (a title stays as a line of text).
- Typing in a large document no longer parses the whole text on every keystroke: the editor now uses the incremental parser, so a keystroke in a 60 000-line document costs milliseconds instead of a quarter of a second.

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

[0.1.0]: https://github.com/isolmaz/ss-writer/releases/tag/v0.1.0
