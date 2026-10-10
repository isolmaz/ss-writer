# SS Writer — Product Specification

Single source of truth for behavior. Code, tests, `README.md`, `docs/features.toml` and the website MUST agree with this file. When behavior changes, this file changes in the same commit.

- Name: **SS Writer** — "Markdown & Text Editor". Binary `SSWriter.exe`.
- Platform: Windows 11 22H2 (build 22621) or later, x64. Lower builds: installer refuses, the exe shows a message box and exits. No Windows 10 code paths.
- UI language: English. All UI strings come from Fluent `.ftl` files (`assets/i18n/<locale>/*.ftl`); no string literals in UI code. Locale = OS UI language, overridable in Settings; fallback `en`. Numbers/dates formatted with `GetNumberFormatEx`/`GetDateFormatEx` for the active locale.
- Principles: speed, quality, simplicity. The file on disk is the truth; the app never writes non-standard markers into user files. App state lives in `%LOCALAPPDATA%\SS Writer\` (or next to the exe in portable mode).

## 1. Window

```
┌ Title row (Mica Alt): [sidebar toggle] [tabs…] [+]                    [─][□][×] ┐
│ Command row: File Edit Format View          [B I U pill] [inspector toggle]   │
├ Sidebar (Mica) ┬ Editor surface (solid layer, rounded top-left) ┬ Inspector ─┤
│                │        centered text column                      │ (Mica)     │
└────────────────┴──────────────────────────────────────────────────┴────────────┘
                   Status bar (toggle): Ln/Col · words · zoom · encoding · EOL
```

- Custom title bar: tabs live in the title row. Caption buttons drawn by the app (Segoe Fluent Icons E921/E922/E923/E8BB); hovering maximize returns `HTMAXBUTTON` so Snap Layouts appear. Rounded corners (system). Resize borders kept.
- Backdrop: Mica Alt (`DWMSBT_TABBEDWINDOW`). Falls back to solid theme color when transparency effects are off, on battery saver, or when the window is inactive (system behavior respected).
- Theme: follows system light/dark and accent color live (`WM_SETTINGCHANGE "ImmersiveColorSet"`); overridable (System/Light/Dark; accent System/Blue `#1874D3`/custom). High contrast: all colors from `GetSysColor`.
- Fonts: UI Segoe UI Variable; icons Segoe Fluent Icons; code Cascadia Mono (bundled, OFL). Editor text font default Segoe UI Variable Text 16 px, line height 1.75, color tokens per theme.
- Per-monitor DPI v2. All geometry in DIPs.
- Menus (File, Edit, Format, View, Window, Help) are native popup menus; dark menus via uxtheme ordinals 135/136 when available, otherwise light.
- Single-file window (opened from Explorer) starts editor-only. Toggling the sidebar shows the file's parent folder as workspace.

## 2. Editor

### 2.1 Live rendering
- CommonMark-based parsing; rendering is WYSIWYG-hybrid.
- Inline markers (`**`, `_`, `~`, `~~`, `==`, `` ` ``, `[`, `](…)`, `$`) are hidden unless the caret or selection touches the element (inclusive bounds); then they appear dimmed (`marker` color).
- Line-start markers (`#`, `>`, list markers, fences, `$$`, table pipes in raw mode) are visible while the caret is on that line/block.
- Setting "Auto-hide Markdown" off → all markers always visible, dimmed.
- The caret never lands inside a hidden range: entering an element reveals its markers first (relayout), caret positions map through the display↔source map.
- Heading line with caret shows a gutter badge (`H1`…`H6`); clicking opens a level menu.

### 2.2 Typing behaviors
- Enter in list/task item continues the list (same marker, renumbered ordered lists, new task unchecked). Enter on an empty item removes the marker (exits list). Shift+Enter inserts a soft line break inside the item.
- Tab / Shift+Tab in a list item indents/outdents (indent unit = setting: 2, 4 or tab; default "align to content").
- Typing ```` ``` ```` + Enter closes the fence; `$$` + Enter closes the math block.
- Pasting a URL over a selection creates `[selection](url)`. Pasting over a link label replaces the label text.
- Toggle formatting commands wrap/unwrap selection; with empty selection they insert an empty pair with the caret inside, or unwrap the element at the caret.
- Ordered lists renumber after edits that change item order/count.
- Tasks: click box or `Ctrl+.` toggles. Completed tasks move to the end of their list (setting "Sort completed tasks", default on); checked text turns `muted`, no strikethrough.
- Insert date (`F5`): long date in the active locale; escaped if it would start an ordered list (`1\.`).

### 2.3 Paste priority
1. Image only → saved as a file per attachment settings; inserts `![](relative/path.png)`. Each pasted image becomes its own file.
2. Files (`CF_HDROP`) → images embedded (copied into the attachment folder unless they already lie inside the document's folder or workspace), other files linked where they are (attachment chip).
3. VS Code (`vscode-editor-data` format present) or HTML whose whole content is one monospace block → plain text; with several lines it goes inside a fenced block unless the caret is already in one. Text VS Code copied from a Markdown or plain-text file is ordinary text (pasted as it is, never fenced).
4. HTML (`HTML Format`) → converted to Markdown (Word `mso-list`, Google Docs, browsers).
5. Plain text.
- Inside a code block, always plain text. `Ctrl+Shift+V` always plain text. Text copied in SS Writer pastes back exactly (its private `SS Writer Markdown` clipboard format wins over the HTML).
- A document without a file has no attachment folder: pasting a picture asks to save the document first; pasted files are linked by their absolute path.

### 2.4 Selection & navigation
- Click caret, Shift+click extend, double-click word, triple-click paragraph, drag select, drag-drop move (Ctrl = copy) including onto other tabs.
- Word boundaries: Unicode word segmentation.
- Home/End line, Ctrl+Home/End document, Ctrl+Left/Right word, Ctrl+Up/Down paragraph, PgUp/PgDn.
- Ctrl+click on a link opens it.

### 2.5 Undo / redo
- Operation-based; consecutive typing grouped (break on 1 s pause, caret jump, whitespace after word). Every command (format, replace-all, table op, paste) is one step. Undo restores selection.

### 2.6 Spell check
- Windows `ISpellCheckerFactory`; languages from settings (default: OS language). Red squiggle; context menu: suggestions, Add to dictionary, Ignore. Skipped: code, URLs, math, wikilink targets, front matter.

### 2.7 Writing modes
- Typewriter (`F9`): caret line kept at a configurable vertical position (default 50 %) with a dead zone (default ±10 % of viewport) so clicks don't jump the view; overscroll past end. `Ctrl+L` recenters.
- Focus (`Shift+F9`): non-current paragraphs dimmed.
- Full screen (`F11`), Zen (`Shift+F11`): full screen, sidebar/inspector/toolbars hidden.
- Zoom: `Ctrl+=`/`Ctrl+-`/`Ctrl+wheel`, reset `Ctrl+0`; app-wide, persisted.

### 2.8 Folding
- Hovering a heading shows a chevron in the left gutter; click folds until the next heading of the same or higher level; folded region shows a `…` pill. Search hits inside a fold unfold it. State stored in the app DB per file path, never in the file. View → Fold all / Unfold all. No default shortcut (assignable).

### 2.9 Formatting UI
- B I U pill (top-right of editor): reflects state at caret; applies to selection or inserts pair. Hidden in Zen. View → toggle.
- Selection toolbar: appears after a mouse selection near it; hides on typing, scroll, Esc. Items: Heading▾, Task, List▾ (bullet/numbered/task), Bold, Italic, Highlight▾ (colors), Link, Table, Image, ⋮ (Underline, Strikethrough, Inline code, Code block, Quote, Callout, Math, Divider, Footnote, Date).

## 3. Markdown elements

| Element | Syntax | Rendering |
|---|---|---|
| Headings | `#`…`######` (ATX only; Setext not supported) | sizes 1.75/1.45/1.25/1.1/1/1 × body, semibold |
| Emphasis | `*x*`/`_x_`, `**x**`/`__x__`, `~x~` underline, `~~x~~` strike | |
| Highlight | `==x==`, colored `=={red}x==` (red, orange, yellow, green, blue, purple) | rounded pill per line fragment |
| Inline code | `` `x` `` | mono, `code_bg` rounded |
| Lists | `-`/`*`/`+`, `1.`/`1)` | accent dot; nested ring, square |
| Tasks | `- [ ]`, `- [x]` | custom checkbox (rounded square) |
| Quote | `>` | left bar |
| Callout | `> [!NOTE|TIP|IMPORTANT|WARNING|CAUTION] Optional title` | icon + color + title; click icon to change type |
| Code block | fenced ```` ``` ````/`~~~` with info string | `code_bg`; hover: language label + Copy button; optional syntax highlighting |
| Math | `$x$`, `$$…$$` | vector (Typst via MiTeX); errors and unsupported commands show the source in `error` color; works inside tables |
| Mermaid | fenced `mermaid`; `.mmd` files | vector diagram below the source while the caret is inside, diagram only otherwise; export PNG/SVG. Diagram types the renderer cannot draw correctly (`sankey-beta`, `block`) and any render failure show the source as a code block with an "Unsupported diagram" note |
| Table | GFM pipes, alignment row | rounded border, bold header, zebra rows; in-cell editing (see 3.1) |
| Image | `![alt](path "title")`, size `![alt|300](p)` / `![alt|300x200](p)` / `<img src width height>` | block if alone in paragraph else inline; resize handles write `|W` |
| Attachment | link to a local non-image file | chip: shell icon, file name, TYPE pill, `size · date` |
| Link | inline, reference (`[x][id]`, `[id]: url`), autolink | accent text; ✎ when caret inside → edit popup (text, URL) |
| Wikilink | `[[target]]`, `[[target|alias]]`, `[[#heading]]`, `[[target#heading]]` | accent text; `[[` opens completion |
| Footnote | `[^id]` + `[^id]: text` | superscript; hover shows text; click jumps |
| Divider | `---`, `***`, `___` | hairline |
| Front matter | `---` YAML at file start (TOML `+++` shown as code) | muted mono card; setting to hide |
| Page break | `<!-- pagebreak -->` | "Page break" hairline label; real break in PDF/DOCX/ODT/print |
| Raw HTML | any other HTML | shown as dimmed raw text (not rendered) |
| Escapes, hard breaks | `\*`, two spaces/backslash at EOL | CommonMark |

### 3.1 Tables
- Rendered grid; caret inside a cell edits that cell's source text. Tab/Shift+Tab next/previous cell (Tab in last cell adds a row). Enter moves down (in the last row it adds a row).
- Cell context menu: insert row above/below, column left/right, delete row/column, align left/center/right, Copy table as Markdown/CSV/HTML, Edit as raw Markdown.
- On leaving the table, pipes are re-aligned (setting "Format tables on exit", default on).
- Tables the cell model cannot represent (escaped `|`, `|` inside inline code spanning cells) open in raw mode automatically.

### 3.2 Links & wikilinks
- Relative `.md` links open inside the app (same tab; Ctrl+click new tab). Web links open in the default browser. Executables (per `AssocIsDangerous`) require confirmation.
- `[[` completion: fuzzy match over workspace documents; `[[#` lists headings of the current file; `|` alias.
- Clicking a wikilink to a missing file offers to create it (only inside the workspace root; paths are canonicalized and rejected if they escape the root).
- Renaming/moving a file inside the workspace offers to update links pointing to it (wikilinks and relative Markdown links).
- Backlinks listed in the Inspector Info tab.

### 3.3 Images & attachments
- Local images decoded with WIC (PNG, JPEG, GIF first frame, BMP, TIFF, WebP, HEIC when codec present, ICO); SVG via usvg. Max width = text column; aspect preserved.
- Remote `https` images loaded by default (no cookies, image MIME only, 20 MB cap, 15 s timeout), cached on disk (500 MB LRU). `http` blocked with a "Click to load" placeholder. Per-document "Don't load remote images" banner action; global setting.
- Attachment folder setting: next to the file (default) | `assets/` | `<file>_assets/` | custom folder. Naming template default `image-{n}`.

## 4. Files

### 4.1 Formats

| Format | Open | Save | Notes |
|---|---|---|---|
| `.md` `.markdown` `.mdx` | ✓ | ✓ | Markdown mode |
| `.txt` and other text (`.log` `.csv` `.json` `.yaml` `.ini` `.toml` source code…) | ✓ | ✓ | Plain mode: no rendering, wrap toggle, optional "Render as Markdown" |
| `.mmd` | ✓ | ✓ | Mermaid source + live diagram |
| `.textbundle` `.textpack` | ✓ | ✓ | |
| `.docx` | ✓ | ✓ | import to Markdown; "Save back to Word" |
| `.odt` | ✓ | ✓ | same model as docx |
| `.rtf` | ✓ | ✓ | same model |
| `.doc` (Word 97–2003) | ✓ | — | import only; save as `.docx` or `.md`. Encrypted files and Word 6/95 files are rejected with a message |

- Import reports what could not be represented (merged cells, headers/footers, columns, text boxes, comments, tracked changes, VML shapes, unsupported equations) in a banner with a details list. OMML equations convert to LaTeX for fractions, scripts, radicals, n-ary operators, delimiters, matrices; others become `[equation]` placeholders and are reported.

### 4.2 Encodings & line endings
- Detection order: BOM → UTF-16/32 null-byte heuristic → valid UTF-8 → `chardetng` (legacy code pages incl. Windows-1254). Encoding and BOM are preserved on save.
- Status bar encoding menu (click the encoding): **Reopen with encoding** decodes the file on disk again (asks before it discards unsaved changes; disabled for a document without a file) and **Save with encoding** writes the document in another encoding at the next save (UTF-8 with or without BOM, UTF-16, UTF-32, legacy code pages). Both are disabled for documents that a converter writes (DOCX, ODT, RTF, TextBundle). The current choice is marked.
- Line endings: stored as `\n` internally; original style restored on save. Mixed files are normalized to the majority style and the user is notified once. Status bar line-ending menu (click the EOL): LF, CRLF or CR for the next save, current style marked; disabled for documents that a converter writes.

### 4.3 Saving
- Autosave default on: 1 s after typing stops, on tab switch, window deactivate and close. Off → classic `Ctrl+S`, dirty dot on the tab, close prompt **Save / Don't save / Cancel**.
- Atomic write: temp file in the same folder → `ReplaceFileW` (keeps attributes/ACLs). Network share failure → temp + `MoveFileExW(MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)`. `ERROR_SHARING_VIOLATION` → retry with backoff (50, 100, 200, 400, 800 ms) then banner.
- Own writes are recognized (expected size, mtime, content hash) and never trigger the external-change path.
- Read-only files (attribute or ACL): banner "This file is read-only" + "Allow editing" (clears the attribute when permitted).
- External change: no local unsaved edits → reload preserving caret/scroll (diff-based); otherwise conflict banner: Keep mine / Load theirs / Compare.
- OneDrive placeholders (`RECALL_ON_DATA_ACCESS`/`RECALL_ON_OPEN`): never hydrated by indexing/search; cloud icon in the tree; opening hydrates with progress.

### 4.4 New & untitled
- `Ctrl+N` in a workspace creates `Untitled.md` (`Untitled 2.md`…) in the selected folder and starts inline rename. Without a workspace: unsaved buffer kept in the session (backed up every 30 s) until saved.
- Opening a file (dialog, command line, Explorer drop, Jump List, Quick Open, sidebar) while the active tab is an unused untitled buffer (never edited, empty, no undo history) replaces that tab instead of adding a new one; an untitled tab with text, or with edits that were undone or deleted again, stays.
- Setting "Sync file name with first heading" (default off).

### 4.5 Version history
- Snapshots in the app DB, deduplicated by content hash: every 5 min while edited, on close, before reloading external changes, before saving back to Word/ODT/RTF. Retention 30 days (setting). History panel: list, diff against current, restore.

### 4.6 Crash recovery
- Worker panics drop only that task. UI-thread panic → minidump (`MiniDumpWriteDump`) + emergency save of dirty documents to the recovery folder; next launch offers recovery. Local rolling log (5 × 1 MB). No telemetry.

## 5. Workspace & sidebar
- Open folder: `Ctrl+Shift+O`, drag a folder onto the window, `SSWriter.exe <folder>`, Explorer "Open with SS Writer" on folders. First run creates `Documents\SS Writer\` with `Welcome.md` (localized).
- One root per tab; the sidebar shows the active tab's workspace. Header: root name, ⋯ menu, + menu (New file, New folder).
- Tree: lazy per folder; live updates (`ReadDirectoryChangesW`). Sorting via `StrCmpLogicalW`: Name, Date modified, Date created, Kind; ascending/descending; Folders first. Filters: Show extensions, Show hidden items, Hide images, Hide non-Markdown files, Show modified date under names. Collapse all / Expand all.
- Non-Markdown files are dimmed; double-click opens them in the default app. Images open in an in-app viewer tab.
- Context menu: Open, Open in new tab, Open in new window, Rename (`F2`), Duplicate, Delete (Recycle Bin via `IFileOperation`), Reveal in Explorer, Copy path, Copy relative path, New file/folder here.
- Drag & drop inside the tree moves (Ctrl copies), Explorer → tree copies/moves by Windows rules (Shift move, Ctrl copy, default by volume). Tree → editor inserts a link. Tree → Explorer copies.
- Index (background, SQLite): file list, headings, links for Quick Open, wikilinks, backlinks, rename updates. Respects `.gitignore` and hidden-item settings. Case-insensitive ordinal file matching (`CompareStringOrdinal`).

## 6. Tabs, windows, session
- Tabs: title (file stem; extension shown for non-Markdown), dirty dot (autosave off), close on hover, middle-click close, drag reorder, drag out → new window, drop onto another window's tab row → move. Context: Close, Close others, Close to the right, Copy path, Reveal in Explorer.
- `Ctrl+T` new tab, `Ctrl+W` close, `Ctrl+Tab`/`Ctrl+Shift+Tab`, `Ctrl+PgDn`/`Ctrl+PgUp`, `Ctrl+Shift+T` reopen closed. Window → Merge all windows.
- Same file in several tabs/windows shares one document (instant sync).
- Single instance (named mutex + `WM_COPYDATA`); new launches open tabs in the last active window (or a new window for folders).
- Session restore: windows (placement), tabs, workspaces, caret, scroll, folds. Per-file caret/scroll remembered for reopened files.

## 7. Search
- Find bar (`Ctrl+F`, floating top-right of the editor): Match case, Whole word, Regex; highlights all matches; `n/m` counter; `Enter`/`F3` next, `Shift+Enter`/`Shift+F3` previous; `Esc` closes. Matches in hidden markers reveal them; in folds unfold.
- Replace (`Ctrl+H`): Replace, Replace all (one undo step), regex capture groups `$1`.
- Go to line (`Ctrl+G`).
- Quick Open (`Ctrl+E`): fuzzy files (recent first); `#` headings in current doc; `>` commands. Turkish i/İ/ı/I equivalence in matching.
- Command palette (`Ctrl+Shift+P`, the same list as Quick Open's `>` mode): every command with its shortcut. Commands that do not apply to the active document (for example Save back to Word on a Markdown file) are listed disabled and do not run; the enablement is the one the menus use.
- Search in folder (`Ctrl+Shift+F`): sidebar panel; case/word/regex; results grouped by file with line previews; click opens at match. Honors `.gitignore`; skips binaries; decodes legacy encodings.

## 8. Inspector & status bar
- Inspector (`Ctrl+Shift+I`), right panel, two tabs:
  - **Contents**: collapsible heading tree with staircase hairlines; current section highlighted; click scrolls and flashes the heading; level filter (H1–H6).
  - **Info**: words, characters (with/without spaces), paragraphs, sentences, reading time (default 200 wpm, setting); selection-aware; file path, size, created, modified, encoding, line endings; backlinks.
- Status bar (View toggle): Ln/Col, words, zoom %, encoding, EOL (encoding/EOL clickable).

## 9. Copy, share, export, print
- `Ctrl+C` puts Markdown text (`CF_UNICODETEXT`), `HTML Format` and `Rich Text Format` on the clipboard (local images of the selection are embedded); `Ctrl+X` does the same and removes the selection. Edit → Copy as: Plain text (markers stripped), Markdown, Rich text (RTF + HTML, plain text as fallback), HTML (also as text). Documents that are not Markdown copy their text only.
- Share (`DataTransferManager` via `IDataTransferManagerInterop`): shares the file.
- Export (File → Export): PDF, PNG, JPG, HTML (single file, embedded CSS/images, math as SVG), ePub, DOCX, ODT, RTF, TXT, Markdown with attachments, TextBundle. Options: page size (A4/Letter), margins, base font size, page numbers, black text, light theme, include title. PDF/PNG/JPG through Typst. Mermaid exportable as PNG/SVG from its context menu.
- Print (`Ctrl+P`): system print dialog; pages from the Typst layout drawn as vectors through `ID2D1PrintControl`.
- Long exports show taskbar progress (`ITaskbarList3`).

## 10. Settings (`Ctrl+,`)
Stored in `settings.json` (human-editable; unknown keys preserved).
- **General**: language; on startup (restore session | new document | default folder); default folder; autosave on/off + delay; new file location; sync file name with heading; check for updates.
- **Appearance**: theme; accent; Mica on/off; sidebar font size; show status bar; show B I U pill.
- **Editor**: text/heading/code fonts; size; line height; paragraph spacing; line width; list indent (align | 2 | 4 | tab); auto-hide Markdown; typewriter position + dead zone; spell check + languages; sort completed tasks; syntax highlighting; format tables on exit; reading speed.
- **Sidebar**: sort, folders first, show extensions, hidden items, hide images, hide non-Markdown, modified date.
- **Attachments**: location, naming template, load remote images.
- **Export**: page size, margins, page numbers, black text, default format.
- **Shortcuts**: every command, rebindable; conflicts flagged.
- **Advanced**: index limits (max files, default 100 000), history retention, open logs folder, reset settings.

## 11. Keyboard
- No default shortcut uses Ctrl+Alt (AltGr). If Ctrl+Alt+key produces a character in the active layout (`ToUnicodeEx` with flag `0x4`), it is text, never a command.
- Shortcuts are stored as virtual key + modifiers and displayed with `GetKeyNameTextW`.

| Command | Default |
|---|---|
| New file / New window | Ctrl+N / Ctrl+Shift+N |
| Open file / Open folder | Ctrl+O / Ctrl+Shift+O |
| Save / Save as | Ctrl+S / Ctrl+Shift+S |
| Print | Ctrl+P |
| Close tab / Reopen closed tab / New tab | Ctrl+W / Ctrl+Shift+T / Ctrl+T |
| Next / previous tab | Ctrl+Tab, Ctrl+PgDn / Ctrl+Shift+Tab, Ctrl+PgUp |
| Undo / Redo | Ctrl+Z / Ctrl+Y, Ctrl+Shift+Z |
| Paste plain | Ctrl+Shift+V |
| Find / Replace / Go to line | Ctrl+F / Ctrl+H / Ctrl+G |
| Find next / previous | F3 / Shift+F3 |
| Quick Open / Command palette / Search in folder | Ctrl+E / Ctrl+Shift+P / Ctrl+Shift+F |
| Bold / Italic / Underline | Ctrl+B / Ctrl+I / Ctrl+U |
| Highlight / Strikethrough | Ctrl+Shift+H / Ctrl+Shift+X |
| Inline code / Code block | Ctrl+Shift+C / Ctrl+Shift+K |
| Link / Task toggle | Ctrl+K / Ctrl+. |
| Heading 1–6 (again → paragraph) | Ctrl+1 … Ctrl+6 |
| Sidebar / Inspector | Ctrl+Shift+E / Ctrl+Shift+I |
| Typewriter / Focus | F9 / Shift+F9 |
| Full screen / Zen | F11 / Shift+F11 |
| Insert date | F5 |
| Zoom in / out / reset | Ctrl+= / Ctrl+- / Ctrl+0 |
| Recenter caret | Ctrl+L |
| Settings | Ctrl+, |
| Next pane | F6 |

## 12. Windows integration
- File associations registered per user by the installer (`OpenWithProgids`; Settings button opens `ms-settings:defaultapps`). Folder verb "Open with SS Writer".
- Jump List: recent files/folders, "New window".
- Explorer preview pane handler (`ssw-preview.dll`, `IPreviewHandler`) for `.md`/`.markdown`/`.mmd`.
- DirectManipulation for touchpad/touch panning with inertia; animated mouse wheel; pen acts as pointer (barrel button = right click). Touch keyboard appears on tap into text (TSF + UIA).
- TSF text store for IME, dictation (`Win+H`), emoji panel (`Win+.`), reconversion.
- UI Automation for every control: editor as Document with TextPattern; tree, tabs, buttons, menus with proper roles. Hidden system caret (`CreateCaret`/`SetCaretPos`) tracks the drawn caret. Honors `SPI_GETCLIENTAREAANIMATION`. Full keyboard navigation (F6 cycles panes).

## 13. Distribution & updates
- Per-user MSI (WiX), portable zip (a `portable` file next to the exe stores settings/data beside it), winget manifest. Releases on GitHub.
- Update check: weekly, `https://api.github.com/repos/<owner>/ss-writer/releases/latest`; notification only; can be turned off.

## 14. Performance targets (measured, not assumed)
- Window frame with Mica backdrop visible < 150 ms from process start; first content frame bounded by Direct3D device creation (205–370 ms measured on the reference machine; created in parallel with startup work) — measured with ETW.
- Input-to-photon ≤ 2 frames — PresentMon + app ETW markers.
- 60 000-line document: open < 300 ms, scrolling at display refresh rate.
- 10 000-file folder: tree visible < 300 ms; index in background.
