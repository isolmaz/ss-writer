# SS Writer — Manual Test Checklist

The automated suite (`cargo xtask coverage`, mutation tests, `cargo xtask perf`) proves behavior that a process can observe about itself. By design it **never** shows or activates a window, never injects input (`SendInput`, `SetCursorPos`, pointer injection), never opens system UI (print dialog, share sheet, file pickers, Explorer) and never touches the user's registry, theme or layout. Everything such tests cannot prove is listed here. Structure follows [`spec.md`](spec.md); item text is derived from the spec and from the "manual-only" notes in the work logs.

## When to run

- **Before every release** (tag `v*`): run the complete list on the release candidate build, installed from the MSI produced by `cargo xtask dist` (sections 1-15), plus the portable zip (13.4-13.8).
- After a change that touches the platform layer (`ssw-win`, TSF, DirectManipulation, printing, shell integration, installer, preview handler, icons) run at least the affected section(s).
- Spec sections 1-13 are verified on the **installed** build. Section 14 uses a release build.

## Test environment

Use a clean Windows 11 22H2 (build 22621) or later x64 machine or VM with a normal user account (not admin). Ideally cover two configurations:

| Config | Setup |
|---|---|
| **A (primary)** | Windows 11 latest, light theme, 100 % DPI single monitor, English UI, mouse + keyboard |
| **B (secondary)** | Windows 11 22H2, dark theme, two monitors (100 % and 150 % or 200 %), touch screen or touchpad with precision drivers, pen if available, Japanese (or Chinese/Korean) IME installed, a second keyboard layout (Turkish Q) installed |

Prepare once:

- A folder `T:\ws` with: `a.md` (headings H1-H6, lists, tasks, table, code block, math, mermaid, callout, footnote, front matter, images), `b.md`, `notes.txt`, `legacy.doc`, `sample.docx`, `sample.odt`, `sample.rtf`, a `sub\` folder, a few PNG/JPG/SVG images, a non-Markdown file (`data.csv`), a read-only file, and a 60 000-line Markdown file (`cargo xtask perf` creates one in `target/perf/fixtures`).
- Files in UTF-8 with and without BOM, UTF-16 LE, Windows-1254 (Turkish), and one with mixed LF/CRLF endings.
- A folder on a network share (SMB) and, if possible, a OneDrive folder with online-only files.
- Tools: PresentMon, Windows Performance Recorder/Analyzer, Narrator, Magnifier, Accessibility Insights for Windows, Process Monitor/Explorer (read-only use), Notepad, Word.

## How to record results

1. Copy this file into the release notes draft or open a GitHub issue titled `Manual test vX.Y.Z` and paste it. Do **not** commit ticked boxes to the repository.
2. Fill in the header block below. Tick `[x]` when the *Expected* result was observed exactly. For a failure write `[!]` and append ` — FAIL: <what happened>`; file one issue per failure and link it. For a skipped item (hardware missing) write `[-]` and the reason.
3. A release requires: no `[!]` items open, and every `[-]` accepted by the release owner in writing in the issue.
4. Attach screenshots/CSV files only where an item says "attach".

```
Version / commit:
Installer (MSI sha256 / zip sha256):
Tester / date:
Machine A: Windows build, GPU, display(s), DPI:
Machine B: Windows build, GPU, display(s), DPI, input devices, IME:
```

Item format: `- [ ] **ID** Steps → Expected`. Steps assume the installed build with default settings unless stated. "Light/dark" refers to Windows personalization (Settings → Personalization → Colors), changed by the tester.

---

## 1. Window (spec §1)

- [ ] **WIN-01** Launch `SSWriter.exe` from the Start menu on a machine with Mica-capable GPU/drivers → the window frame shows the Mica Alt backdrop (wallpaper tint visible through title row and sidebar), editor surface is a solid rounded-top-left layer; no white/black flash before the first frame.
- [ ] **WIN-02** Turn off Settings → Personalization → Colors → Transparency effects while the app runs → backdrop changes live to a solid theme color; turn on again → Mica returns. Repeat with Battery saver (Settings → System → Power) → solid fallback.
- [ ] **WIN-03** Click another window so SS Writer is inactive → backdrop follows system inactive-window behavior (solid/less tinted), and returns when reactivated.
- [ ] **WIN-04** App Settings → Appearance → Mica off → solid backdrop immediately; on → Mica again, without restarting.
- [ ] **WIN-05** Switch Windows between Light and Dark while the app is open (also with accent color changed) → app theme and accent update live without restart: title row, tabs, sidebar, editor, scrollbars, status bar, popup menus, tooltips and dialogs all follow; no element stays in the old theme.
- [ ] **WIN-06** Settings → Appearance: theme System/Light/Dark and accent System/Blue (`#1874D3`)/custom → each choice applies instantly; with "System" the OS accent change (Settings → Colors → Accent color) is picked up live.
- [ ] **WIN-07** Dark title bar: with Windows in dark mode, the caption buttons, title text and window border are dark and legible; in light mode light. Inactive window title is dimmed but legible.
- [ ] **WIN-08** Caption buttons render with Segoe Fluent Icons glyphs (minimize E921, maximize E922, restore E923, close E8BB): hover/pressed/focus states, close button turns red on hover with white glyph, correct glyph swap (maximize ↔ restore) when maximized by double-clicking the title row, Win+Up, or Snap.
- [ ] **WIN-09** Hover the maximize button for ~1 s (mouse) → Snap Layouts flyout appears; choose a layout cell → window snaps; the other zone offers other windows. Also open with Win+Z and snap with Win+Arrow keys → works; maximized window has no gap/overflow at screen edges, no clipped tab row.
- [ ] **WIN-10** Drag the title row (empty area between tabs and caption buttons) → window moves; double-click the empty area → maximize/restore; right-click empty title area → system menu (Restore/Move/Size/Minimize/Maximize/Close) appears at the pointer; Alt+Space opens the same menu at the top-left.
- [ ] **WIN-11** Resize from all 8 edges and corners (including the top edge in normal and maximized state); cursors correct; minimum size is enforced and layout does not break; rounded corners are system-drawn (Windows 11) and not cut off by content.
- [ ] **WIN-12** Drag the window between two monitors with different DPI (100 % ↔ 150 %/200 %) → text, icons, hairlines, tab heights and the caret re-render crisp at the new DPI with no blur, no stretched frame, no layout jump; drag mid-way (straddling) → no flicker or black areas. Repeat by moving with Win+Shift+Arrow.
- [ ] **WIN-13** Change display scaling in Windows Settings while the app is open (e.g. 100 → 125 %) → app rescales without restart (sign-out prompt is not required for the app itself).
- [ ] **WIN-14** Unplug or disable the monitor the window is on (or change resolution) → window is moved onto a visible monitor; closing and relaunching restores it on-screen. Restore a window that was last on a now-missing monitor → appears on the primary monitor fully visible.
- [ ] **WIN-15** High contrast: enable a contrast theme (Aquatic, Desert, Night sky, and the default "High Contrast Black/White") with Alt+Left Shift+PrtScn → every surface, text, selection, caret, tab, link, button, focus rectangle, task checkbox, code block and menu uses system colors (no hard-coded light/dark colors left, nothing illegible); disabling returns to the normal theme live. Mica is not shown.
- [ ] **WIN-16** Fonts: UI text is Segoe UI Variable, icons Segoe Fluent Icons, code Cascadia Mono (bundled — verify on a machine where Cascadia Mono is NOT installed). Editor text defaults to Segoe UI Variable Text 16 px with 1.75 line height. No fallback boxes (tofu) in UI chrome.
- [ ] **WIN-17** Native popup menus: open File, Edit, Format, View, Window and Help by mouse, by Alt+F / Alt+E / Alt+O (Format) / Alt+V / Alt+W / Alt+H, and by Alt or F10 alone followed by arrow keys → each menu appears below its command-row item with the commands of §11 (File: New file, New window, Open file, Open folder, Reopen closed tab, Save, Save as, Save back to Word, Version history…, Export ▸ (Export as PDF … TextBundle), Export…, Print, Share, Settings, Exit; Edit: Undo/Redo, Cut/Copy/Paste/Paste as plain text/Select all, Copy as ▸, Insert date, Find…, Quick Open, Command palette, Search in folder; Format; View; Window: New window, Merge all windows (disabled while no other window is open), New tab, Close tab, Reopen closed tab, Next tab, Previous tab; Help: Welcome document, Keyboard shortcuts, Report an issue, Check for updates, About SS Writer); accelerator text matches the keymap (`GetKeyNameTextW`), check marks (Sidebar, Inspector, Status bar, Formatting pill, Typewriter/Focus…) and submenus work, commands that need a document are disabled with none open, Esc closes, menu follows dark/light theme (dark when uxtheme ordinals 135/136 are available; light otherwise — verify no unreadable dark-on-dark text either way).
- [ ] **WIN-18** Menu appearance in dark mode on the oldest supported build (22H2, build 22621) and on the newest available build → menus either fully dark or fully light, never mixed; popup context menus (editor, tree, tab) behave the same.
- [ ] **WIN-19** Open a single file from Explorer (double-click `a.md`) → window opens editor-only (no sidebar); toggle the sidebar (Ctrl+Shift+E) → file's parent folder appears as workspace.
- [ ] **WIN-20** Launch on a Windows build lower than 22621 (or simulate via a VM with an older build) → the exe shows the message box "SS Writer needs Windows 11 version 22H2 (build 22621) or later. This computer runs build N." and exits; the MSI refuses to install (launch condition). *(Skip with reason if no such VM is available; do not alter the registry of a shared machine.)*
- [ ] **WIN-21** Minimize, restore from the taskbar, maximize, full screen (F11) and back → no black frames, content repaints immediately, taskbar thumbnail and Alt+Tab preview show the correct content. Taskbar button hover thumbnail shows the window, not an empty rectangle.
- [ ] **WIN-22** Window with the "Show window contents while dragging" disabled (Advanced system settings → Performance) → still moves/resizes correctly (outline mode); with animations disabled (Settings → Accessibility → Visual effects → Animation effects off) the app's animations (smooth caret, flash, transitions) are skipped (`SPI_GETCLIENTAREAANIMATION`).
- [ ] **WIN-23** Run the app on a GPU-less/basic-render VM (WARP) and, if available, on a machine with an integrated + discrete GPU; simulate device loss (disable/enable the display adapter in Device Manager) → app recovers (re-creates devices) without crash or blank window.
- [ ] **WIN-24** Lock the workstation (Win+L) and unlock; sleep/resume; RDP connect/disconnect → window repaints correctly, no frozen frame, caret and blink resume.

## 2. Editor (spec §2)

### 2.1 Live rendering and typing

- [ ] **ED-01** Visual review of live rendering on config A and B in light and dark: headings, bold/italic/underline/strike, highlights (all six colors), inline code, links, quotes, callouts, tables, task boxes, dividers → alignment, spacing, anti-aliasing (ClearType/grayscale as designed), color contrast and marker dimming look correct and consistent with the design; no clipped glyph ascenders/descenders, no 1 px gaps between highlight pills.
- [ ] **ED-02** Move the caret with arrow keys through `**bold**` and a link → markers reveal when the caret touches the element (inclusive bounds) and hide when it leaves; caret never appears inside a hidden range; no jitter or text reflow flash visible during reveal.
- [ ] **ED-03** Caret blinking: caret blinks at the system blink rate (Settings → Accessibility → Text cursor; and Control Panel → Keyboard "Cursor blink rate"); the caret indicator thickness/indicator setting from Windows Accessibility is honored if implemented; caret stops blinking while typing and while the window is inactive, and resumes.
- [ ] **ED-04** Heading line with caret shows `H1…H6` gutter badge; click the badge → level menu; pick a level → line updates, one undo step.
- [ ] **ED-05** Enter/Tab/Shift+Tab/Shift+Enter in nested lists and tasks by real typing (physical keyboard, key auto-repeat held down on Enter and on Backspace) → no dropped or doubled input, no lag.
- [ ] **ED-06** Click a task box with the mouse and press Ctrl+. → toggles; completed tasks slide to the end of the list with a smooth (or, with animations off, immediate) transition; checked text uses the muted color without strikethrough.
- [ ] **ED-07** F5 inserts the long date in the active locale; switch Windows regional format/language to Turkish and German (Settings → Time & language) and restart → the date and number formatting follow.
- [ ] **ED-08** Hold Ctrl+Z / Ctrl+Y (auto-repeat) over a long typing history → smooth, selection restored at each step.

### 2.2 Clipboard and paste (real applications)

- [ ] **ED-10** Copy formatted text from Word (bold, list with `mso-list`, table, headings) and paste (Ctrl+V) → converted to Markdown matching the visible structure; `Ctrl+Shift+V` pastes plain text only.
- [ ] **ED-11** Copy from Google Docs and from a Chromium page and a Firefox page (headings, links, lists, table, image) → converted to Markdown; images follow the attachment settings.
- [ ] **ED-12** Copy a single multi-line snippet in VS Code (from a source file) → pasted inside a fenced code block unless the caret is already in one; a single-line snippet → inline plain text; text copied from a `.md`/`.txt` file in VS Code → pasted as-is, never fenced.
- [ ] **ED-13** Copy an image (screenshot with Win+Shift+S; image from Paint; image from browser "Copy image") → pasting creates a file in the attachment folder with the naming template (`image-1`, `image-2`…) and inserts `![](relative/path.png)`; paste two different images consecutively → two separate files.
- [ ] **ED-14** In Explorer, copy image files and non-image files (Ctrl+C) and paste → images are embedded (copied to the attachment folder unless inside the document/workspace folder), other files appear as attachment chips linking to where they are.
- [ ] **ED-15** In an untitled document with no file, paste a screenshot → app asks to save the document first; paste files → linked by absolute path.
- [ ] **ED-16** Paste a URL over a selected word → `[word](url)`; over a link label → label replaced.
- [ ] **ED-17** Select text in SS Writer, Ctrl+C, paste into SS Writer elsewhere → exact roundtrip (private `SS Writer Markdown` format). Paste the same into Word → formatted rich text (headings, bold, lists, tables, images embedded); into Notepad → Markdown text; into Outlook/Gmail compose box → formatted HTML.
- [ ] **ED-18** Ctrl+X, then Edit → Copy as ▸ (Copy as plain text, Copy as Markdown, Copy as rich text, Copy as HTML) → each result verified by pasting into Notepad (plain/Markdown/HTML-as-text) and Word (rich text). Plain-text mode (`.txt`) copy puts text only. Right-click in the editor shows Cut, Copy (disabled without a selection), Paste, Select all.
- [ ] **ED-19** Windows clipboard history (Win+V) shows SS Writer copies, and pasting from history works; copy a large selection (5 MB) → no UI freeze, no truncation.
- [ ] **ED-20** Copy from SS Writer while another application holds the clipboard open (e.g. a clipboard manager running) → retry works and no error is shown or data lost.

### 2.3 Selection, mouse, drag

- [ ] **ED-30** Mouse: click, Shift+click, double-click (word), triple-click (paragraph), drag-select, drag past the edge (autoscroll with speed proportional to distance) → correct selection; double-click on CJK/Turkish/emoji text selects by Unicode word boundaries.
- [ ] **ED-31** Drag selected text to a new place (move) and with Ctrl held (copy; cursor shows `+`); drop onto another tab in the tab row → document switches/drops according to spec and text lands in the target document; Esc during drag cancels.
- [ ] **ED-32** Drag text from SS Writer to Notepad/Word and from Notepad/Word into SS Writer → drop effect cursors correct (move/copy), content arrives intact (HTML from Word converts to Markdown).
- [ ] **ED-33** Ctrl+click on a link opens it; hovering a link shows hand cursor; plain click places the caret and shows the ✎ edit popup.
- [ ] **ED-34** Mouse wheel scrolling: smooth animated scroll with a standard wheel, with a high-resolution wheel (Logitech/Razer free-spin), with "Scroll inactive windows when I hover over them" on/off, and with Settings → Mouse → "Lines to scroll" at 1 and 100 → each follows the setting; Shift+wheel = horizontal where applicable; Ctrl+wheel = zoom.
- [ ] **ED-35** Cursor shapes in the editor: I-beam over text, arrow over the gutter, scrollbar, image and diagram bodies, dividers and the empty area below the last block, hand over links (with Ctrl), task boxes, fold chevrons, the code block's Copy button and a footnote with a definition, resize cursor on image handles, `⇔` on sidebar/inspector splitters.
- [ ] **ED-36** Cursor shapes outside the editor: move the pointer from the text to the title bar, the tab row and its close buttons, the B/I/U pill, the sidebar rows, the status bar segments, a banner's buttons, the find bar and the Settings window → the I-beam never stays behind; every clickable thing shows the hand, text boxes the I-beam, inert chrome the arrow.

### 2.4 Touch, pen, touchpad

- [ ] **ED-40** Precision touchpad: two-finger pan scrolls with smooth inertia and no overshoot at document start/end (or a subtle rubber-band as designed); pan horizontally on a wide table; pinch zoom changes the zoom (status bar percentage) with clamped limits and persists app-wide.
- [ ] **ED-41** Touch screen: one-finger drag scrolls with inertia; tap places the caret; tap a task box toggles it; long-press shows the context menu; two-finger pinch zooms; no accidental text selection while panning; the sidebar tree and inspector list scroll by touch as well.
- [ ] **ED-42** Touch selection: long-press + drag selects text; selection handles (if shown) work; the selection toolbar appears near the selection.
- [ ] **ED-43** Pen: hover shows the caret/pointer; tap places the caret; draw-select selects text; scroll with pen (drag on scrollbar and, if the device supports it, pen-flick); barrel button press → context menu (right-click); eraser end (if present) does not crash or select.
- [ ] **ED-44** Touch keyboard: on a tablet-mode/touch device tap into the editor → the touch keyboard appears automatically (and hides when focus moves to a non-text control); the editor content scrolls so the caret is not hidden behind the keyboard; typing works; with a hardware keyboard attached and "Show the touch keyboard when there's no keyboard attached" setting off → keyboard does not appear. Verify also in the Find bar and the Settings text fields.
- [ ] **ED-45** Mixed input: while the pen is hovering, use the mouse; while a touch pan inertia is running, press a key or click → inertia stops, no jump in scroll position.
- [ ] **ED-46** Touchpad/mouse during modal UI (open dialog, open menu) → scroll/gestures do not leak to the editor behind.

### 2.5 Text services (IME, dictation, emoji, reconversion)

- [ ] **ED-50** Japanese IME (Microsoft IME, Hiragana): type `nihongo` in a paragraph → composition string shows inline with the underline style of the IME, the candidate window appears directly below the composition (above if near the screen bottom), follows the caret when the text scrolls or the window moves, and sits on the correct monitor at 150 % DPI. Space cycles candidates, Enter commits → the committed text is inserted exactly once; Esc cancels → text removed.
- [ ] **ED-51** Composition inside a **list item**, a **task item**, a **table cell**, a **heading** and a **blockquote** → candidate window and underline appear at the right place; markers (`- `, `[ ]`, `|`) are not damaged; committing with Enter does NOT also trigger list continuation (the first Enter commits, the second continues the list).
- [ ] **ED-52** Composition at the start/end of a hidden-marker element (inside `**bold**` while the caret reveals markers) → composition does not shift or flash; cancel and commit both leave a valid Markdown source; undo after commit removes the whole committed string in one step.
- [ ] **ED-53** Selection replaced by composition: select a word, start typing with IME → the selected text is replaced when the composition starts; cancel restores correct state (no loss of the original text beyond the replaced selection per normal editors).
- [ ] **ED-54** Chinese (Pinyin, Wubi) and Korean (Hangul, with Hanja conversion) IMEs → compose, convert, commit; Korean composition of a syllable block while the caret is moved by arrow keys → commits correctly with no duplicate character.
- [ ] **ED-55** Reconversion: select a committed Japanese word and press the IME Convert key (or Ctrl+/ with the Microsoft IME) → composition restarts on the selected text with candidates.
- [ ] **ED-56** IME mode indicator (taskbar `あ`/`A`): focusing the editor enables the IME and switching input language with Win+Space/Alt+Shift works while typing; the IME state is per-thread and the Find bar/Quick Open text fields accept IME input as well.
- [ ] **ED-57** On-screen emoji panel (`Win+.`): open at the caret, insert an emoji, a kaomoji and a symbol → inserted at the caret, panel positioned at the caret; for a ZWJ emoji sequence the caret moves over the whole cluster with one arrow key press; Backspace deletes it at once.
- [ ] **ED-58** Voice typing/dictation (`Win+H`): dictate a sentence with the caret in a paragraph, in a list item and in a table cell → text appears at the caret, partial text updates in place, punctuation is inserted ("period", "new line"), the dictation toolbar stays attached to the window; stop dictation with Win+H → no stray characters.
- [ ] **ED-59** Handwriting panel (pen/touch keyboard handwriting) → input appears at the caret; Windows Text Suggestions and "Multilingual text suggestions" (touch keyboard) do not corrupt text.
- [ ] **ED-60** Turkish Q keyboard: `ı İ i I ğ ş ç ö ü` typed with the Turkish layout and AltGr combinations (e.g. `AltGr+Q` = `@`, `AltGr+E` = `€`) produce text, never trigger commands. German layout: `AltGr+7` = `{`, `AltGr+Q` = `@`, Polish `AltGr+A` = `ą` → all insert characters. Dead keys (French `^` + `e`, `´` + `a`) compose correctly. Test the same in the Find bar.
- [ ] **ED-61** Clipboard/IME: during an IME composition press Ctrl+V / Ctrl+Z / the mouse in another place → composition is committed (or canceled, per IME) first; no corrupted state or crash.
- [ ] **ED-62** With the Japanese IME on, record a shortcut in Settings → Shortcuts by pressing a key combination → the combination is captured as a shortcut (virtual key + modifiers) and not consumed by the IME.

### 2.6 Spell check, writing modes, folding

- [ ] **ED-70** Spell check with the OS language (and a second language added in Settings → Editor): misspelled words show red squiggles; right-click → suggestions apply; Add to dictionary persists (check it appears in Windows Settings → Time & language → Typing → Spelling → "Manage dictionary" or in the custom dictionary) and removes squiggles in other tabs; Ignore hides it for the session. Squiggles are absent in code, URLs, math, wikilink targets and front matter.
- [ ] **ED-71** Typewriter mode (F9): the caret line stays near the configured vertical position (default 50 %); clicking inside the dead zone (±10 %) does not move the view; arrow-key travel and typing keep the line fixed; overscroll past end works; Ctrl+L recenters. Change the position/dead zone in Settings → Editor and see it applied live.
- [ ] **ED-72** Focus mode (Shift+F9): non-current paragraphs are dimmed, dim fade is smooth; contrast of the current paragraph is unchanged; works with the caret in lists, tables and code blocks.
- [ ] **ED-73** Full screen (F11): window covers the monitor including the taskbar, restore returns to the previous placement; auto-hide taskbar does not overlap; Zen (Shift+F11): sidebar, inspector, toolbars, tabs and B I U pill are hidden, Esc/Shift+F11 leaves; works on the secondary monitor and with a monitor at 150 % DPI.
- [ ] **ED-74** Zoom: Ctrl+= / Ctrl+- / Ctrl+0 / Ctrl+wheel / touchpad pinch → zoom steps are smooth and crisp at 50 % … 300 % (text re-laid out, not bitmap-scaled), status bar shows percentage, zoom persists after restart and across all windows.
- [ ] **ED-75** Folding: hover a heading → chevron appears in the left gutter; click → fold with `…` pill; click pill → unfold; View → Fold all/Unfold all; search hit inside a fold unfolds it; close and reopen the file → fold state restored from the app DB; the `.md` file is unchanged on disk (compare bytes).
- [ ] **ED-76** Selection toolbar: select text with the mouse → toolbar appears near the selection (above/below depending on space, within the work area on multi-monitor); hides on typing, scroll and Esc; every item works (Heading▾, Task, List▾, Bold, Italic, Highlight▾, Link, Table, Image, ⋮ overflow); keyboard/touch selection does not show it; with Narrator running the toolbar is reachable.
- [ ] **ED-77** B I U pill (top right of the editor): reflects the state at the caret, applies to selection or inserts a pair; hidden in Zen; View toggle hides it.

## 3. Markdown elements (spec §3)

- [ ] **MD-01** Render check in a gallery document (`a.md` with every element of the §3 table) in light, dark and high contrast → every element has the specified look: heading sizes 1.75/1.45/1.25/1.1/1/1, list dots/rings/squares, rounded checkboxes, quote bar, callout icon/color/title for NOTE/TIP/IMPORTANT/WARNING/CAUTION, code block background, divider hairline, muted front-matter card, dimmed raw HTML.
- [ ] **MD-02** Callout: click the callout icon → menu to change type; the type in the source updates.
- [ ] **MD-03** Code blocks: hover → language label + Copy button appear; Copy puts the code (without fences) on the clipboard; syntax highlighting on/off in Settings applies live; ligatures/glyphs of Cascadia Mono are correct; a 5000-line block scrolls without lag.
- [ ] **MD-04** Math (`$x$`, `$$…$$`, and inside tables): vector rendering is crisp at 100 %, 200 % zoom and 200 % DPI; baseline aligns with the surrounding text; an error shows the source in the `error` color; formulas follow light/dark text color.
- [ ] **MD-05** Mermaid (`flowchart`, `sequence`, `class`, `state`, `gantt`, `pie`, `er` samples): caret inside the fence → source plus the diagram below; caret outside → diagram only; diagrams are readable (text not clipped, arrowheads correct) in light and dark; `sankey-beta` and `block` show the source code block with the "Unsupported diagram" note; a broken diagram shows the same note instead of crashing. Right-click → export PNG/SVG → files open correctly in an image viewer/browser.
- [ ] **MD-06** `.mmd` file opens with Mermaid source + live diagram and saves back unchanged.
- [ ] **MD-07** Tables: look (rounded border, bold header, zebra rows) in all themes; Tab/Shift+Tab/Enter navigation; cell context menu (rows, columns, alignment, Copy as Markdown/CSV/HTML — paste into Excel to verify CSV/HTML, Edit as raw Markdown); mouse drag to select cells; table wider than the column scrolls/fits as designed; leaving a table re-aligns pipes (when the setting is on); IME composition inside a cell (see ED-51).
- [ ] **MD-08** Images: PNG, JPEG, GIF, BMP, TIFF, WebP, ICO, SVG render; HEIC renders when the HEIF codec is installed (Microsoft Store extension) and shows the placeholder when not; resize handles drag smoothly and write `|W` into the source; image alone in a paragraph is a block, otherwise inline; ICC/EXIF-rotated JPEG is shown upright.
- [ ] **MD-09** Remote images: `https` images load (placeholder "Loading image…" first); `http` shows the "Click to load" placeholder and loads on click; a dead URL shows "Image could not be loaded"; the banner "Remote images are loaded" with its "Don't load remote images" action and the global Settings → Attachments → "Load remote images" block loading; offline → placeholder, no hang; a cached image shows when offline after a first load.
- [ ] **MD-10** Attachment chip: link to a local non-image file shows the shell icon (matches Explorer's icon for that type, sharp at 100 % and 200 % DPI), name, TYPE pill and `size · date`; clicking opens it in the default app; for executable content (e.g. `.exe`, also written `.exe.`) nothing happens (`AssocIsDangerous`).
- [ ] **MD-11** Links (Ctrl+click): a relative `.md` or `.txt` link opens in a new tab, or activates its tab when it is open already (a `#heading` suffix is ignored, the file opens at the top); a missing one shows the "not found" error; a relative link to an image or PDF in the document's folder or a subfolder opens the default app; `..\x.pdf`, an absolute or `\\server\share` path to such a file, `tool.exe` and `tool.exe.` do nothing; `https://` link opens the default browser (try Edge and Firefox as default); `mailto:` opens the mail client; `file:` and other schemes do nothing.
- [ ] **MD-12** Wikilinks: typing `[[` opens completion; fuzzy match; `[[#` lists headings; `|` alias; completion popup is positioned at the caret, stays inside the work area, and follows when scrolling; choose with Enter/Tab/mouse click.
- [ ] **MD-13** Footnotes: hover a `[^1]` reference → popup shows the text; click jumps to the definition and back.
- [ ] **MD-14** Page break `<!-- pagebreak -->` shows the "Page break" hairline label; real break appears in the exported PDF and in printing (see §9).
- [ ] **MD-15** Front matter YAML card is shown muted mono; setting to hide works; TOML `+++` shown as code.
- [ ] **MD-16** Emoji, CJK, RTL (Arabic/Hebrew) and combining characters in a paragraph, heading, table cell and list → correct shaping, color emoji drawn in color, bidi order correct, caret movement by clusters, selection rectangles continuous, line height not disturbed by fallback fonts.
- [ ] **MD-17** Very wide or deep content: 200-column table, 30-level nested list, 1 MB single-line paragraph → no crash; UI remains responsive.

## 4. Files (spec §4)

### 4.1 Opening and formats

- [ ] **FILE-01** File → Open file (system file dialog titled "Open file"): the dialog is the modern Windows common dialog with the filters "Markdown" and "All files"; multi-select opens each file in a tab; dialog starts in the current file's folder/last used folder; cancel does nothing. File → Open folder ("Open folder") shows the folder picker.
- [ ] **FILE-02** Save as (system dialog): default file name and folder are sensible, extension appended, overwrite prompt appears, saving to a folder requiring elevation shows a clear error and keeps the document dirty; Save as for Word/ODT/RTF offers matching types.
- [ ] **FILE-03** Open `.docx`, `.odt`, `.rtf`, `.doc` created by Word (headings, lists, tables, images, footnotes, equations, comments) → imports to Markdown; banner lists what could not be represented (merged cells, headers/footers, columns, text boxes, comments, tracked changes, VML shapes, unsupported equations); details list opens; an encrypted `.doc` and a Word 6/95 `.doc` are rejected with a message.
- [ ] **FILE-04** "Save back to Word": edit an imported `.docx` and save → open the result in Word: no repair prompt ("unreadable content"), structure/styles preserved, images present; same for ODT (Word) and RTF (Word/WordPad). A snapshot is created in history before the save.
- [ ] **FILE-05** `.doc` save: Ctrl+S on an imported `.doc` does not write a `.doc`; Save as offers `.docx`/`.md` and the saved copy opens in Word/the app; the message shown ("Imported documents are saved with Save as, or saved back to their original format." / "This format cannot be written.") is readable.
- [ ] **FILE-06** `.textbundle`/`.textpack` open and save (verify with another TextBundle app or by unzipping the pack).
- [ ] **FILE-07** Plain mode for `.txt`, `.log`, `.csv`, `.json`, `.yaml`, `.ini`, `.toml`, source files: no rendering; a 50 MB log opens without freezing the UI. View → Word wrap (checked by default, also in the command palette, no default shortcut) is enabled only for plain files: switched off, long lines stay on one row and the text scrolls sideways with Shift+wheel, a horizontal wheel/touchpad swipe, the caret (End/Home) and Narrator's scroll pattern; switched on, they wrap at the window edge; the choice is per tab. View → Render as Markdown (checkable, enabled only for plain files) shows the file as live Markdown (headings in the inspector Contents) and disables Word wrap; the tab title keeps the `.txt` name, and after an edit and Ctrl+S the file on disk keeps its name, encoding, BOM and line endings (compare bytes); switching it off shows plain lines again. Both items are disabled for `.md`, `.mmd`, Word files and untitled documents.
- [ ] **FILE-08** Binary or non-text file via Open → clear message, no crash.

### 4.2 Encodings and line endings

- [ ] **FILE-10** Open UTF-8 (with/without BOM), UTF-16 LE/BE, UTF-32, Windows-1254 (Turkish `ığüşöç` correct), Windows-1252, Shift-JIS, GBK files → text correct, status bar shows the encoding; edit and save → the bytes outside the edit are preserved, BOM and encoding kept (compare with a hex viewer or `fc /b`).
- [ ] **FILE-11** Click the encoding in the status bar → a popup with the submenus "Reopen with encoding" and "Save with encoding" (UTF-8, UTF-16 LE/BE, UTF-32 LE/BE, Windows-1250…1258, ISO-8859-x, KOI8, Shift_JIS, EUC-JP/KR, GBK, GB18030, Big5; the current encoding marked; "Save with encoding" also offers "UTF-8 with BOM"). "Reopen with encoding" decodes the file again (a Windows-1251 file shows `Привет` after choosing windows-1251; with unsaved edits it first asks "Reopening … discards your unsaved changes. Continue?" and No keeps the edits); "Save with encoding" marks the document modified and the next save writes the chosen encoding and BOM (check the bytes with a hex viewer). Both submenus are disabled for imported documents (DOCX/ODT/RTF/TextBundle) and "Reopen with encoding" is disabled for an untitled buffer. Saving a Unicode document as Windows-1254 with an unrepresentable character warns.
- [ ] **FILE-12** CRLF file stays CRLF after saving, LF stays LF; mixed file shows the one-time notification and is normalized to the majority style; clicking the line ending in the status bar (LF / CRLF / CR) opens a popup with LF, CRLF and CR (the current style marked; all disabled for imported documents); choosing one marks the document modified and the next save writes that style (check the bytes with a hex viewer).

### 4.3 Saving, external changes

- [ ] **FILE-20** Autosave on: type, stop → file on disk updates ~1 s later (watch with Explorer/Notepad reload/`dir` time); switching tabs, deactivating the window (Alt+Tab) and closing also save. Autosave off: dirty dot on the tab, Ctrl+S saves, closing prompts Save / Don't save / Cancel (verify all three buttons, Esc = Cancel).
- [ ] **FILE-21** Window close (X), Alt+F4, taskbar "Close window", and Windows shutdown/restart/sign-out with a dirty document → no data lost: autosave on saves; autosave off prompts, and on forced shutdown the recovery data exists after the next launch. Shutdown is not blocked indefinitely by the app.
- [ ] **FILE-22** Save over a network share (SMB): works; disconnect the network mid-session (disable adapter) and save → clear banner, no data loss, retry works after reconnect; edit a file open in another program that holds it exclusively (e.g. Word) → retry with backoff then banner.
- [ ] **FILE-23** Atomic write preserves attributes/ACLs: set a custom ACL / hidden attribute on a file, edit and save → ACL and attributes unchanged; a file in a OneDrive/Dropbox-synced folder saves without sync conflicts ("conflicted copy").
- [ ] **FILE-24** Read-only file (attribute) → banner "This file is read-only" + "Allow editing" clears the attribute; read-only by ACL (deny write) → banner, Allow editing explains the failure if not permitted.
- [ ] **FILE-25** External change with no unsaved edits: modify the open file in Notepad and save → SS Writer reloads silently keeping caret and scroll; with local unsaved edits (autosave off) → conflict banner Keep mine / Load theirs / Compare; each button works; a snapshot is written before reloading.
- [ ] **FILE-26** SS Writer's own saves never produce a reload or conflict (watch the banner area during typing with autosave on, including with antivirus real-time scanning active).
- [ ] **FILE-27** OneDrive Files On-Demand: a folder with online-only files → sidebar shows the cloud icon and indexing/search/workspace open do NOT download them (check in OneDrive that files stay online-only); opening one hydrates it with a progress indicator and then opens it; cancel works.
- [ ] **FILE-28** Open a file from a path longer than 260 characters, with Unicode/emoji in the name, with trailing space/dot-less names, on a UNC path, on a removable drive (and remove the drive while open → banner, not crash).
- [ ] **FILE-29** Windows file-lock behaviors: the same file open in two SS Writer windows stays in sync instantly (type in one, see the other); Explorer cannot delete the file only while it is being written (no lingering lock after the save).

### 4.4 New, untitled, history, recovery

- [ ] **FILE-30** Ctrl+N in a workspace creates `Untitled.md` (`Untitled 2.md`…) in the selected folder with the inline rename box focused; typing a name and Enter renames; Esc keeps the default. Without a workspace: untitled buffer; kill the process from Task Manager ("End task") after 40 s → relaunch restores the unsaved buffer (30 s backup).
- [ ] **FILE-31** Setting "Sync file name with first heading" on → renaming follows the first heading and the file name in Explorer changes.
- [ ] **FILE-32** File → Version history… opens the "Version history" panel: "Saved versions" list (entries `date hh:mm UTC — size`), selecting one shows "Changes since this version" (added/removed lines legible in light/dark and high contrast; "This version is identical to the current document." when equal), "Restore this version" restores and is undoable, Close works; the empty state reads "There are no saved versions of this document yet."; history survives restart; the conflict banner's Compare opens it with the "File on disk" entry.
- [ ] **FILE-33** Crash recovery: start the app, edit without saving (autosave off), then force a crash if a debug hook is available in the release candidate; otherwise End-task the process → relaunch shows the banner "Recovered unsaved work — SS Writer closed unexpectedly. Restore the recovered text?" with Restore / Discard; Restore → content restored; Discard (or closing the banner) asks "Discard recovered work" first and then removes the recovery files. Minidump (`*.dmp`) and rolling log (5 × 1 MB) exist under `%LOCALAPPDATA%\SS Writer\` after a crash; no network traffic occurred (verify with Resource Monitor/Wireshark that there is no telemetry apart from the update check).
- [ ] **FILE-34** Antivirus: install with Microsoft Defender real-time protection on → no false positive on the exe/MSI/DLL; SmartScreen on the downloaded MSI: record what the user sees ("Unknown publisher" is expected without code signing) and that "Run anyway" works.

## 5. Workspace and sidebar (spec §5)

- [ ] **WS-01** First run on a fresh profile → `Documents\SS Writer\` created (also when Documents is redirected to OneDrive) with a localized `Welcome.md`, which opens.
- [ ] **WS-02** Open a folder via Ctrl+Shift+O (system folder picker), via dragging a folder from Explorer onto the window, via `SSWriter.exe "T:\ws"` from a command prompt, and via Explorer context menu "Open with SS Writer" on a folder (also on a folder background-less selection of multiple folders) → workspace opens in each case; the Explorer verb shows the SS Writer icon.
- [ ] **WS-03** Sidebar toggle (Ctrl+Shift+E), resize splitter by mouse and by keyboard, width persists across restarts; collapse animation respects Animation effects setting.
- [ ] **WS-04** Tree: expand folders lazily with a 10 000-file folder (no UI freeze, scrolling at refresh rate); file icons are drawn at the right DPI; selected/hover/focus visuals in light/dark/high contrast; keyboard (arrows, Home/End, Enter, F2, Delete, type-ahead).
- [ ] **WS-05** Live updates: create/rename/delete/move files in Explorer (and with `git checkout`, a sync client, and a bulk copy of 5000 files) → the tree updates within a second without flicker or losing selection/expansion.
- [ ] **WS-06** Sorting: Name uses natural order (`file2` before `file10`, as `StrCmpLogicalW`), Date modified, Date created, Kind, asc/desc, Folders first; filters: Show extensions, Show hidden items (verify with an item with the Hidden attribute set in Explorer), Hide images, Hide non-Markdown, modified date under names; Collapse all/Expand all.
- [ ] **WS-07** Non-Markdown files are dimmed; double-click opens in the default app (e.g. `.csv` → Excel); images open in an in-app viewer tab (zoom/pan by mouse wheel, touch pinch, touchpad).
- [ ] **WS-08** Tree context menu on a file: Open, Open in new tab, Open in new window, Rename (F2), Duplicate, Delete, Reveal in Explorer, Copy path, Copy relative path, New file here, New folder here (on a folder the three Open entries are absent; on empty space only Reveal in Explorer, Copy path, New file/folder here). Delete asks "Move <name> to the Recycle Bin?" and then the item is in the Recycle Bin (verify, and that "Restore" in Explorer works); Reveal opens Explorer with the item selected; Copy path / Copy relative path verified by pasting in Notepad; Shift+F10 and the Menu key open the menu from the keyboard. Sidebar header: "New" (+) menu offers New file/New folder, "More options" (⋯) offers Collapse all/Expand all. Right-click while a file is being renamed does not break state.
- [ ] **WS-09** Rename/move with link updates: rename a file referenced by wikilinks and relative links from other documents → the offer dialog appears; accept → links updated (verify files); decline → untouched; rename into/out of the workspace works as specified.
- [ ] **WS-10** Drag & drop inside the tree: move by default, Ctrl copies (cursor shows `+`), cannot drop a folder into itself; dropping onto a collapsed folder after a hover delay expands it (springs open); autoscroll while dragging near the edges.
- [ ] **WS-11** `DoDragDrop` from Explorer → tree: no modifier = copy across volumes / move on the same volume, Shift = move, Ctrl = copy; cursor tooltips ("Move to …"/"Copy to …") match; a name collision shows the Windows "Replace or skip" dialog; drop a read-only/locked file → Windows error UI appears, app remains stable. Drop on the editor from Explorer inserts a link/embeds per paste rules.
- [ ] **WS-12** Tree → editor drag inserts a relative link at the drop point (image files insert `![]()`); tree → Explorer window/desktop copies the file (drop onto the desktop and onto a folder in another Explorer window); tree → another application (Notepad, Word, mail attachment) works as a file drop.
- [ ] **WS-13** Index: respects `.gitignore` and hidden settings (a file listed in `.gitignore` does not appear in Quick Open/search); Quick Open on a 10 000-file folder appears instantly; wikilink completion works while the index is still building; case-insensitive matching with non-ASCII names (`İstanbul.md` vs `istanbul.md`).
- [ ] **WS-14** One root per tab: switching to a tab with a different workspace changes the sidebar content (state preserved per tab); closing a workspace's last tab clears it.
- [ ] **WS-15** Workspace on a network share and on a removable drive with slow I/O → app stays responsive; disconnecting shows an error state, not a hang.

## 6. Tabs, windows, session (spec §6)

- [ ] **TAB-01** Tab visuals: titles (stem; extension shown for non-Markdown), dirty dot with autosave off, close button on hover, middle-click closes, text ellipsis at narrow widths, overflow with many tabs (scroll/chevron), `+` button; active tab has the correct Mica/solid layering in all themes.
- [ ] **TAB-02** Drag reorder: drag a tab left/right → neighbors animate aside, drop commits; Esc during the drag cancels; works by touch and pen.
- [ ] **TAB-03** Drag a tab out of the window (down/away from the tab row) → a new window is created under the pointer with that document, the source window loses the tab; the last tab dragged out of a window closes/handles the source window per design; works onto another monitor and with different DPI.
- [ ] **TAB-04** Drag a tab onto another SS Writer window's tab row → the tab moves there (insertion position follows the pointer) and the source window loses it; Task Manager shows exactly one `SSWriter.exe` process for all windows.
- [ ] **TAB-05** Tab context menu (right-click, Shift+F10): Close tab, Close other tabs, Close tabs to the right (disabled when not applicable), Copy path, Reveal in Explorer (both disabled for untitled documents); keyboard: Ctrl+T, Ctrl+W, Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+PgDn/PgUp, Ctrl+Shift+T reopens the last closed (repeatedly, in order).
- [ ] **TAB-06** Window → Merge all windows (also in the command palette, Ctrl+Shift+P → "Merge all windows"): the menu item is disabled while only one window is open; with three windows open, all tabs gather into the active window (documents shared, nothing lost, unsaved text kept) and the other windows close.
- [ ] **TAB-07** Same file in two tabs/windows shares one document (type in one, the other updates instantly, caret independent); close one → no data lost.
- [ ] **TAB-08** Single instance: with SS Writer running, double-click a `.md` in Explorer, run `SSWriter.exe file.md` from a command prompt, and open from the Jump List → the file opens as a tab in the **last active** window and that window is brought to the foreground (taskbar does not just flash; if foreground is denied by Windows it flashes — record behavior); launching with a **folder** opens a new window; no second process remains afterward (check Task Manager). Launch 5 files at once (select 5 in Explorer, Enter) → all open as tabs, in order, once each.
- [ ] **TAB-09** Single instance across **elevation and sessions**: launching from an elevated prompt while the non-elevated instance runs → the new launch behaves sensibly (opens a new elevated window or shows an error; no hang); fast user switching (another session) does not forward into the other user's instance.
- [ ] **TAB-10** Launch while the existing instance is minimized → it restores; while on another virtual desktop → record behavior (switches to its desktop or opens a new window) and verify no hang.
- [ ] **TAB-11** Session restore (Settings → On startup = restore session): arrange 2 windows on 2 monitors (one maximized), several tabs, workspaces, carets, scroll positions, folds; quit and relaunch → the same windows at the same placements (monitor, size, maximized), tabs, workspaces, caret/scroll/folds restored; a file deleted in the meantime is skipped gracefully; a window whose monitor is gone appears on the primary monitor. Other startup modes: new document, default folder.
- [ ] **TAB-12** Per-file caret/scroll: close a file in a long document, reopen it later → returns to the same position.
- [ ] **TAB-13** Windows restart / sign-out with the app open (Restart Apps setting "Automatically save my restartable apps") → after sign-in the app does not break; session restore works.
- [ ] **TAB-14** Unused Untitled tab: in a window whose active tab is an untouched "Untitled" (new, empty, never edited), open a file by File → Open file, Quick Open, the sidebar, dropping a file from Explorer and by launching `SSWriter.exe file.md` again → the file takes the Untitled tab's place (no extra "Untitled" tab remains). An Untitled tab that holds text, was edited and emptied again (undo history), or is not the active tab stays beside the new tab; opening a file that is already open only activates its tab.

## 7. Search (spec §7)

- [ ] **SRCH-01** Find bar (Ctrl+F) floats at the top-right of the editor; options Match case, Whole word, Regex; all matches highlighted; `n/m` counter; Enter/F3 next, Shift+Enter/Shift+F3 previous, Esc closes and returns focus to the editor with the caret at the current match; the bar is positioned correctly at 200 % DPI, zoomed, and in a narrow window; IME and AltGr input work in the field; focus ring visible.
- [ ] **SRCH-02** Matches inside hidden markers reveal them; matches inside folds unfold; current match scrolls into view centered with a flash; highlight colors legible in light/dark/high contrast.
- [ ] **SRCH-03** Replace (Ctrl+H): Replace, Replace all (one undo step), regex capture groups `$1` → verify results; Replace all across a 60 000-line document completes without freezing the UI.
- [ ] **SRCH-04** Go to line (Ctrl+G) overlay (placeholder "Line number, 1 to N"): type a number → jumps and flashes; invalid input shows "Not a valid line number"; Esc closes.
- [ ] **SRCH-05** Quick Open (Ctrl+E): fuzzy file match, recent files first; `#` headings of the current doc; `>` commands; Turkish i/İ/ı/I equivalence (type `istanbul` finds `İstanbul.md` and vice versa); arrow keys, Enter, Esc, mouse and touch selection; overlay centered, within the work area, correct at 200 % DPI and on the second monitor; the list scrolls with the wheel/touchpad.
- [ ] **SRCH-06** Command palette (Ctrl+Shift+P): every command listed with its current shortcut (reflects rebinding in Settings); executing a command works; commands unavailable in the context are listed in the disabled text color and Enter or a click on them does nothing (the palette stays open), exactly as the same items are greyed in the menus — e.g. with a plain `.md` document "Save back to Word" is disabled, and with an imported `.docx` it is enabled (the same in Quick Open after typing `>`).
- [ ] **SRCH-07** Search in folder (Ctrl+Shift+F): sidebar panel "Search in folder" with case/word/regex; "Searching…" then "N results in M files" grouped by file with line previews; "No results found" and "Invalid regular expression" states; "Only the first N are shown" for a huge result set; click opens at the match (and reveals/unfolds); honors `.gitignore`; skips binaries; finds text in Windows-1254/UTF-16 files; 10 000-file folder search stays responsive and can be refined while running.
- [ ] **SRCH-08** Screen-reader pass of the Find bar, Quick Open and palette: announced as search/edit controls, result count and selection changes are announced (see §15).

## 8. Inspector and status bar (spec §8)

- [ ] **INSP-01** Inspector (Ctrl+Shift+I) opens as a right panel with Mica; Contents tab: collapsible tree with staircase hairlines, current section highlighted while scrolling (including while using touchpad inertia), click scrolls and flashes the heading, level filter H1-H6 works.
- [ ] **INSP-02** Info tab: words, characters (with/without spaces), paragraphs, sentences, reading time (change reading speed in Settings), selection-aware (select text → values change), file path/size/created/modified (compare with Explorer properties), encoding, line endings, backlinks (click opens the linking file). Numbers use the active locale's digit grouping (e.g. `1.234` in German, `1 234` in French).
- [ ] **INSP-03** Status bar (View toggle): Ln/Col, words, zoom %, encoding, EOL update live while typing/selecting; encoding and EOL are clickable and open the menus; hidden when toggled off; text readable at 200 % DPI; not covered by the taskbar in full screen.
- [ ] **INSP-04** Inspector and sidebar splitter drag; widths persist; Inspector content scrolls with touch/touchpad.

## 9. Copy, share, export, print (spec §9)

- [ ] **EXP-01** Share (File → Share, `DataTransferManager`): the Windows share sheet opens, anchored to the SS Writer window (not centered on the screen), lists targets (Nearby sharing, Mail, Teams, copy link…); share the file to Mail → the attachment is the file with the right name and content; with an untitled/unsaved document → the message "Save the document first: only files can be shared.".
- [ ] **EXP-02** Export each format, both via File → Export… (dialog "Export" with Format list, Options, Export / Cancel buttons) and via File → Export ▸ (Export as PDF, PNG, JPG, HTML, ePub, DOCX, ODT, RTF, TXT, Markdown with attachments, TextBundle) → Save dialog (default name/folder; TextBundle package/folder filters), result opens correctly in the matching external viewer: PDF in Edge/Acrobat (fonts embedded, vector text selectable, math as vectors, page numbers, margins, Letter vs A4), PNG/JPG in Photos, HTML in Edge (single file; embedded CSS/images; math as SVG) and in Firefox, ePub in Edge/Calibre/Apple Books (validate with EPUBCheck if available), DOCX in Word (no repair prompt), ODT/RTF in Word, TextBundle in a TextBundle-aware app. Exporting over the document itself is refused ("Choose a different file…").
- [ ] **EXP-03** Export options: page size A4/Letter, margins, base font size, page numbers, black text, light theme, include title → each option visibly changes the output in the expected way; "light theme" yields a light PDF while the app is in dark mode; page break `<!-- pagebreak -->` yields a real page break in PDF/DOCX/ODT.
- [ ] **EXP-04** Long export (a 60 000-line document to PDF): taskbar button shows progress (green bar, indeterminate first), error state turns red on failure, the UI stays responsive, the window can be moved; at the end the progress clears and the dialog "Export finished … Open it now? Choose Yes to open the file, No to show it in its folder." appears — Yes opens the PDF in the default viewer, No opens Explorer with the file selected; PNG/JPG export of a multi-page document reports "N pages … exported".
- [ ] **EXP-05** Export to a path that is read-only/locked/nonexistent drive → clear error; no zero-byte file left.
- [ ] **EXP-06** Mermaid context menu export PNG/SVG → files are valid and match the diagram on screen (open in browser and an image viewer, check transparency/background and text rendering).
- [ ] **PRT-01** Print (Ctrl+P): the system print dialog opens in front of the SS Writer window, modal to it, with the default printer, page range, copies, orientation, and paper size options.
- [ ] **PRT-02** Print to a **physical printer** (laser + inkjet if available): output matches the on-screen document layout (page breaks, margins, headings, tables, code blocks, vector math, images, Mermaid diagrams), text is sharp (vector, not bitmapped), page numbers and black-text option behave, multi-page document prints all pages in order, duplex/ collate/ copies are honored.
- [ ] **PRT-03** Print to "Microsoft Print to PDF" and "Microsoft XPS Document Writer" → file opens; page count equals the preview; selecting a page range prints only those pages; Letter/A4 follows the dialog and export setting.
- [ ] **PRT-04** Print Cancel → no output, no leftover spooler job, app stays usable; print with the printer offline → Windows error UI, no hang or crash; the window stays responsive during spooling (the spool job proceeds while editing).
- [ ] **PRT-05** Print a large document (500+ pages) → taskbar progress, memory stays bounded (watch Task Manager), the document can be edited while printing or the app blocks only as designed (record).
- [ ] **PRT-06** Print in dark theme → paper output uses the light/black-text layout (no dark background ink).
- [ ] **PRT-07** Open the print dialog on a touch device (touch only, no mouse) → controls are usable by touch and the dialog can be dismissed.

## 10. Settings and Help (spec §10)

- [ ] **SET-01** Settings (Ctrl+,) opens as a window/page with all eight groups (General, Appearance, Editor, Sidebar, Attachments, Export, Shortcuts, Advanced); layout, tab order and focus rectangles are correct in light/dark/high contrast and at 200 % DPI; all controls are reachable by keyboard.
- [ ] **SET-02** Change every setting once and verify visible effect: language (restart prompt/immediate switch; non-English OS UI language selects that locale or falls back to `en`), startup mode, default folder (folder picker), autosave on/off + delay, new file location, sync file name, check for updates; theme, accent (custom color picker), Mica, sidebar font size, status bar, B I U pill; fonts (the font picker lists installed fonts; the chosen font renders), size, line height, paragraph spacing, line width, list indent (align | 2 | 4 | tab), auto-hide Markdown (off = markers always visible, dimmed), typewriter position/dead zone, spell check + languages, sort completed tasks, syntax highlighting, format tables on exit, reading speed; sidebar sorting/filters; attachments location/naming/remote images; export defaults; Advanced index limits and history retention.
- [ ] **SET-03** Settings are applied live to all open windows and persisted: quit and relaunch → values retained; Advanced → "Open settings.json" opens the file, edit it in Notepad while the app runs and save → changes apply right away, and unknown keys survive an app write (compare the file).
- [ ] **SET-04** Corrupt `settings.json` (invalid JSON) → app starts with defaults, keeps a backup or shows a message, never crashes.
- [ ] **SET-05** Shortcuts page: rebind a command by pressing a key combination → displayed with `GetKeyNameTextW` names in the active language (switch input language to German → "Strg+…"); conflicts are flagged; Ctrl+Alt combos that produce characters on the active layout (AltGr) are rejected or flagged; Reset restores defaults; palette and menus show the new shortcut at once.
- [ ] **SET-06** Settings → Advanced → "Open logs folder" opens Explorer on the folder; "Reset all settings" asks "Reset settings — Put every setting back to its default? Your shortcuts are reset too." and restores defaults on Yes (nothing on No) without touching documents.
- [ ] **SET-07** Default apps (spec §12): Settings (Ctrl+,) → General → "Default app" row with the button "Default apps…" and the hint "Opens Windows Settings, where you choose the app that opens Markdown and text files." → clicking it opens `ms-settings:defaultapps` in Windows Settings (Settings → Apps → Default apps); no SS Writer setting changes, and a keyboard (Tab, Space/Enter) and a screen reader (button named "Default apps…") can use it.
- [ ] **SET-08** Different system locales: UI strings come from `.ftl`, no raw message IDs visible anywhere (menus, dialogs, tooltips, banners, errors); long translations don't truncate; numbers and dates follow the locale (`GetNumberFormatEx`/`GetDateFormatEx`).
- [ ] **HELP-01** Help → Welcome document and Help → Keyboard shortcuts open read-only tabs titled "Welcome" and "Keyboard shortcuts" (the latter lists every command with its current shortcut, reflecting rebinding); they cannot be edited or saved over the user's files, and reopening focuses the existing tab.
- [ ] **HELP-02** Help → About SS Writer shows the version, "Markdown & Text Editor", MIT license and repository link; Yes opens the "Third-party notices" document (fonts, crates, vendored code listed), No closes the box.
- [ ] **HELP-03** Help → Report an issue opens the GitHub issues page (`github.com/isolmaz/ss-writer`) in the default browser.

## 11. Keyboard (spec §11)

- [ ] **KEY-01** Walk the whole §11 shortcut table by hand on the US layout; each default works and appears in the matching menu item and command palette: New file/window, Open file/folder, Save/Save as, Print, Close tab/Reopen/New tab, tab navigation, Undo/Redo (both Ctrl+Y and Ctrl+Shift+Z), Paste plain, Find/Replace/Go to line, Find next/previous, Quick Open/palette/Search in folder, Bold/Italic/Underline, Highlight/Strikethrough, Inline code/Code block, Link/Task toggle, Heading 1-6 (press again → paragraph), Sidebar/Inspector, Typewriter/Focus, Full screen/Zen, Insert date, Zoom, Recenter, Settings, F6.
- [ ] **KEY-02** Repeat KEY-01 on a Turkish Q, German, French AZERTY and a Japanese 106/109 keyboard layout: no shortcut collides with an essential character; characters reachable only via AltGr (`@ € { } [ ] \ |` and `# ~`) can be typed everywhere (editor, Find bar, Quick Open, Settings fields, rename box); shortcut labels in menus show the layout-correct key names (`Ctrl+,` on layouts where the comma needs shift or a different key; `Ctrl+=` and `Ctrl+-` on layouts where `+` requires Shift — zoom still works with `Ctrl+Shift+=`/numpad `+`/`-`).
- [ ] **KEY-03** Dead keys, Num Lock/Caps Lock states, Right-Alt-as-AltGr and left Ctrl+Alt (which Windows treats as AltGr) → no command is triggered by AltGr character combinations.
- [ ] **KEY-04** F6 cycles panes (tabs → sidebar → editor → inspector → …) with a visible focus indicator; Tab/Shift+Tab move inside a pane; Esc returns to the editor from overlays; menu mnemonics Alt+F / Alt+E / Alt+O / Alt+V / Alt+H work; Alt alone or F10 focuses the menu row (Shift+F10 opens the context menu instead).
- [ ] **KEY-05** Key auto-repeat and rapid typing (hold a letter 5 s; mash keys) → no dropped events, no stuck modifiers after Alt+Tab or Win key combos (Ctrl stuck test: Alt+Tab while holding Ctrl, return, type).
- [ ] **KEY-06** Sticky Keys, Filter Keys and Toggle Keys (Windows accessibility) on → shortcuts and typing still work as the OS dictates.
- [ ] **KEY-07** Windows reserved/combined shortcuts: Win+Shift+S, Win+., Win+H, Win+V, Win+Z, Win+Arrows, Alt+Space, Ctrl+Esc and Ctrl+Shift+Esc are not swallowed or broken by the app.
- [ ] **KEY-08** Remote Desktop session and Windows Sandbox: all shortcuts, clipboard, IME and rendering behave (RDP drops GPU features, record any rendering fallback).

## 12. Windows integration (spec §12)

### 12.1 Shell, taskbar, associations

- [ ] **INT-01** File associations after the MSI install (per user): in Explorer, right-click each of `.md .markdown .mdx .mmd .txt .textbundle .textpack .docx .odt .rtf .doc` → "Open with" lists SS Writer (with the document icon), choose it and "Always" → double-click opens in SS Writer; Settings → Apps → Default apps lists "SS Writer" with its capabilities; Settings → General → "Default apps…" in SS Writer opens that page (SET-07). Existing defaults (e.g. Word for `.docx`) are NOT changed by installation.
- [ ] **INT-02** Explorer file icons for associated types show the SS Writer document icon (`.md`, `.markdown`, `.mmd` …) at all Explorer icon sizes (small, medium, large, extra-large) and in dark mode; no blank/default icon after install (and cache rebuilt without a reboot).
- [ ] **INT-03** Explorer folder context menu: "Open with SS Writer" appears on folders (right-click on a folder, and Shift+right-click) with the icon, and opens the folder as a workspace. On Windows 11's new context menu the item is visible under "Show more options" (record) — verify it is not shown for files (as designed).
- [ ] **INT-04** Jump List: right-click the taskbar button → recent files and folders listed (after opening a few), "New window" task opens a new window; pinning to the taskbar then launching from the pinned icon keeps the same taskbar button (AppUserModelID matches the Start menu shortcut; no duplicate taskbar buttons for the shortcut and the running window); removing an item from the Jump List works; "Unpin" works. Recent entries appear in Explorer Quick Access's Recent too if designed that way.
- [ ] **INT-05** Start menu: search "SS Writer" finds the app with the right icon; pin to Start; launch; uninstall removes the entry.
- [ ] **INT-06** Taskbar: icon is crisp at 100/125/150/200 % scaling and in the dark/light taskbar; overlay/progress during export (see EXP-04); thumbnail toolbar not required; grouping with a second window of the app under one button.
- [ ] **INT-07** "Open with" from Explorer for a file path with Unicode/emoji, spaces, and `&`/`%` characters; Send To → SS Writer (if added by the user); drag a file from Explorer onto the SS Writer taskbar icon (opens it in the last active window) and onto the exe in Explorer (command-line args).
- [ ] **INT-08** Drag a file from Explorer onto the title row / tab row / empty window: appropriate behavior (opens as tab) with the drop cursor "Open with SS Writer"; drag a folder → opens as workspace; drag multiple files → tabs; drag a `.lnk` → resolves to the target or opens the shortcut file per design.
- [ ] **INT-09** Drag **from** SS Writer: drag the tab (or a tree item) to Explorer → file copy occurs; drag selected text to the desktop creates a text clipping or is rejected without a hang (record).
- [ ] **INT-10** Update/export notifications (if shown through Windows notifications rather than in-app): appear with the app name and icon, respect Focus assist / Do Not Disturb, and clicking activates the app. Skip with reason if the build only uses in-app banners.

### 12.2 Explorer preview pane

- [ ] **PRV-01** After installation, in Explorer open a folder with `.md`, `.markdown` and `.mmd` files, enable the Preview pane (Alt+P) and select each → the rendered document appears (headings, lists, tables, code, images relative to the file, math, Mermaid diagram); selecting a `.mmd` shows the diagram; no crash/hang of `prevhost.exe`; switching selection rapidly stays responsive.
- [ ] **PRV-02** Preview pane in Windows **dark mode** and **light mode** → background, text color, and font follow Explorer's visuals (`IPreviewHandlerVisuals`); toggling the Windows theme while the pane is open updates (or after reselection); high contrast uses system colors.
- [ ] **PRV-03** Preview pane at 100 % and 200 % DPI and on a second monitor with different DPI; resize the pane and window → content re-lays out crisply without clipping; scrollbars work by mouse wheel, touch and keyboard (focus the preview with Tab).
- [ ] **PRV-04** Preview of large and hostile inputs: 50 MB `.md` (size cap respected, a truncation note or a quick partial render), binary content renamed to `.md`, an empty file, a file with Windows-1254/UTF-16 encoding, a file with remote images (no network requests from the preview unless designed; offline works), a very deep nesting → no crash, no UI freeze in Explorer.
- [ ] **PRV-05** Preview is read-only: typing/selecting text works for copy only; Ctrl+C copies selected text; links do not navigate inside the preview (or open the default browser as designed); no caret is displayed.
- [ ] **PRV-06** Also test the preview from other hosts: the Outlook attachment preview and the Open/Save dialog preview pane (if enabled), plus the file in OneDrive (online-only: preview does not hydrate unexpectedly).
- [ ] **PRV-07** After uninstall: the preview pane for `.md` shows its previous behavior (no stale registration, no "this file can't be previewed" for other apps' handlers); after upgrade the registration remains and `prevhost.exe` released the old DLL (the upgrade does not need a reboot or reports it clearly).

### 12.3 Icons

- [ ] **ICO-01** App icon legibility: inspect `SSWriter.exe` icon at 16, 24, 32, 48 and 256 px (Explorer small/medium/large/extra-large views, taskbar at 100/150/200 %, alt-tab, window caption icon, Start menu, Settings → Apps list, jump list, MSI Add/Remove programs) on light, dark and high-contrast backgrounds → recognizable shape, no blurry/clipped edges, no color mud at 16 px, 256 px looks sharp.
- [ ] **ICO-02** Document icon (`document.ico` for `.md`, `.mmd` etc.) at the same five sizes in Explorer's file list and open dialogs, light and dark Explorer → legible, differs from the app icon, extension badge (if any) readable at 32 px.
- [ ] **ICO-03** High contrast: icons remain visible (no black-on-black) in the taskbar, Explorer and title bar.
- [ ] **ICO-04** File properties of `SSWriter.exe` → Details tab: Product name "SS Writer", description, company, version equal the release version; icon shown in Properties.

### 12.4 Input and accessibility APIs

- [ ] **INT-20** DirectManipulation: see ED-40…ED-46 (touchpad/touch panning with inertia, pinch zoom) — repeat on a second device type (touchscreen vs precision touchpad) and with the display at 60 Hz and ≥ 120 Hz; inertia deceleration feels natural, no judder; panning does not stall when the window is on a secondary monitor.
- [ ] **INT-21** Hidden system caret: with Narrator/Magnifier running, the Magnifier "follow text insertion point" tracks the drawn caret in the editor, including after clicking, arrowing and scrolling; Windows "Text cursor indicator" (Settings → Accessibility → Text cursor) is displayed at the correct position.
- [ ] **INT-22** `SPI_GETCLIENTAREAANIMATION`: toggle Settings → Accessibility → Visual effects → Animation effects while running → smooth scrolling/animated transitions follow the setting (no animation when off).

## 13. Distribution and updates (spec §13)

### 13.1 MSI install (clean machine)

- [ ] **DIST-01** Double-click `SSWriter-<ver>-x64.msi` as a standard (non-admin) user → installs **without UAC prompt** to `%LOCALAPPDATA%\Programs\SS Writer`; SmartScreen/Smart App Control behavior recorded; installer UI (if any) is legible in dark mode and at 200 % DPI.
- [ ] **DIST-02** After install: Start menu shortcut `SS Writer` exists and launches; `SSWriter.exe`, `ssw_preview.dll`, `assets\` (fonts, `i18n`) are present; the app starts and shows bundled Cascadia Mono; first launch creates `%LOCALAPPDATA%\SS Writer\` and `Documents\SS Writer\Welcome.md`.
- [ ] **DIST-03** Add/Remove programs (Settings → Apps → Installed apps) lists "SS Writer" with the right icon, version and publisher; "Modify/Repair" behavior recorded.
- [ ] **DIST-04** File associations and folder verb exist after install (INT-01, INT-03) and the preview handler is registered (PRV-01). In `regedit` (read-only) verify the keys live under HKCU (`Software\Classes\SSWriter.*`, `.md\OpenWithProgids`, `Directory\shell\SSWriter`, `…\PreviewHandlers`) and nothing is written to HKLM.
- [ ] **DIST-05** Silent install `msiexec /i SSWriter-<ver>-x64.msi /qn` and with `/l*v install.log` → installs without UI; exit code 0; log has no errors; silent uninstall `msiexec /x … /qn` → removes everything.
- [ ] **DIST-06** Launch condition: installing on a build < 22621 is refused with the message (VM only, see WIN-20).
- [ ] **DIST-07** Install while SS Writer is running (the app has a file open and `prevhost.exe` holds `ssw_preview.dll`) → the installer handles locked files via Restart Manager (closes/prompts) and completes.

### 13.2 Upgrade and downgrade

- [ ] **DIST-10** Install version N, use the app (settings changed, session with tabs, history, recent files, pinned to taskbar), then install N+1 over it (double-click and `msiexec /i`) → major upgrade removes the old version first and installs the new one; settings, session, history DB, recent files, shortcuts and the taskbar pin survive; file associations still work; preview handler still works; no duplicate Add/Remove entries; the installed version in Settings → Apps is N+1.
- [ ] **DIST-11** Installing an older version over a newer one → blocked with "A newer version of SS Writer is already installed."
- [ ] **DIST-12** Upgrade via `winget upgrade isolmaz.SSWriter` from the previous release (see DIST-41) → same results as DIST-10. Separately, a portable folder next to an installed copy keeps its own data (portable data is never read or modified by the MSI).
- [ ] **DIST-13** Repair: run `msiexec /fa SSWriter-<ver>-x64.msi` (or Repair in Apps) after deleting `ssw_preview.dll` and a registry key → restored.

### 13.3 Uninstall

- [ ] **DIST-20** Uninstall from Settings → Apps and from `msiexec /x` → app files, Start menu shortcut, Jump List entries, file association ProgIDs, `OpenWithProgids` entries, folder verb, preview handler registration (CLSID, `PreviewHandlers`, `shellex`) and `Software\SS Writer\Components` are removed (verify by `regedit` search "SSWriter" and "SS Writer" under HKCU, and that Explorer no longer shows "Open with SS Writer" or a preview); `%LOCALAPPDATA%\SS Writer\` user data is kept (documents are never touched) — record whether the data is kept as designed.
- [ ] **DIST-21** After uninstall, a previously associated `.md` double-click falls back to another registered app (no "file association broken" error) and Explorer icons for `.md` revert to the default for that app.
- [ ] **DIST-22** Uninstall while the app is running → prompts/closes or completes after Restart Manager; no leftover empty install folder.

### 13.4 Portable zip

- [ ] **DIST-30** Extract `SSWriter-<ver>-x64-portable.zip` to a folder on the Desktop and to a USB stick: contains `SSWriter.exe`, `portable` marker, `assets`, `ssw_preview.dll` (if shipped), licenses and `THIRD-PARTY-NOTICES.txt`; double-click `SSWriter.exe` → runs without installing (no UAC).
- [ ] **DIST-31** Portable mode data location: settings, history DB, session, caches, logs and recovery files are created **next to the exe**, not in `%LOCALAPPDATA%\SS Writer\` (verify with Explorer/Process Monitor); delete the `portable` marker → data location switches to `%LOCALAPPDATA%` (and back).
- [ ] **DIST-32** Portable run does not register file associations, folder verb, preview handler or Start menu entries (registry untouched), yet Jump List and taskbar pinning still work with their AppUserModelID; moving the folder to another path keeps settings and session working (relative paths) and the move does not break the Jump List (stale entries are ignored).
- [ ] **DIST-33** Portable on a read-only location (write-protected USB, Program Files) → clear message or fallback; does not crash.
- [ ] **DIST-34** The zip's files are not blocked by Mark-of-the-Web after "Unblock" (if the zip is downloaded, Windows shows "Windows protected your PC" on the exe — record), the exe runs after unblocking.

### 13.5 winget

- [ ] **DIST-40** `winget validate packaging/winget` (the rendered `dist/winget` manifest) → passes; `winget install --manifest dist\winget` on a clean VM (with `winget settings --enable LocalManifestFiles`) → installs per user without UAC, `winget list isolmaz.SSWriter` shows the version; `winget upgrade` shows no upgrade when current.
- [ ] **DIST-41** After the release is published: `winget search "SS Writer"` / `winget install isolmaz.SSWriter` (community repo, after the PR merges) → installs the MSI at the right hash (no hash mismatch); `winget uninstall isolmaz.SSWriter` → clean removal; `winget upgrade isolmaz.SSWriter` upgrades from the previous release (DIST-10 expectations).
- [ ] **DIST-42** Installer hash: `Get-FileHash` of the downloaded MSI/zip equals `InstallerSha256` in the manifest and the published checksum file.

### 13.6 Update check

- [ ] **UPD-01** Update check (weekly when Settings → General → "Check for updates" is on; manually via Help → Check for updates): with a build older than the latest GitHub release → a banner "A new version of SS Writer is available — SS Writer <new> is available. You are using <current>." with a Download button; nothing is fetched before it is pressed; closing the banner dismisses it.
- [ ] **UPD-02** Same version or newer than latest → Help → Check for updates shows "SS Writer <version> is the latest version."; the weekly check shows nothing.
- [ ] **UPD-03** Offline/blocked network/GitHub rate limit (403)/invalid JSON → the weekly check is silent; the manual check shows "SS Writer could not check for updates." with the reason; no hang on exit or startup (check with the network disabled and with a proxy requiring authentication).
- [ ] **UPD-04** Setting off → no request is made (verify with Resource Monitor/Fiddler/Wireshark that nothing connects to `api.github.com`); with it on, only a single request to `api.github.com/repos/<owner>/ss-writer/releases/latest` per week and no other telemetry traffic during a full session.
- [ ] **UPD-05** Download: press Download in the banner of an installed (MSI) build → the banner shows a bar and "x of y MB downloaded" that both grow, the editor stays usable while it downloads, and the package appears in `%LOCALAPPDATA%\SS Writer\updates\SSWriter-<new>-x64.msi` with the SHA-256 of the release's `SHA256SUMS.txt` (`Get-FileHash`). When it is there the banner reads "SS Writer <new> is ready to install" with a "Restart and install" button.
- [ ] **UPD-06** Restart and install: with unsaved work in an open tab, press "Restart and install" → the editor saves its session and closes, the msiexec basic UI runs, SS Writer opens again with the new version (Help → About) and the previous tabs and caret positions; `%LOCALAPPDATA%\SS Writer\updates\` is empty afterwards, no `SSWriter-updater.exe` process is left in Task Manager, the unsaved tab is back with its text, and after closing and starting SS Writer once more `%TEMP%` holds no `SSWriter-update-*` folder.
- [ ] **UPD-07** Postponing: download the update, close the banner, keep working → nothing is installed, the package stays; the offer (with Restart and install) comes back on the next check.
- [ ] **UPD-08** Failures: break the download (disable the network while the bar is moving) → "SS Writer could not download the update." with the reason, no file left in `updates\`, and the banner offers Download again. Then cancel the msiexec UI during an install → the editor is already closed; starting SS Writer again shows the old version and the package is still there.
- [ ] **UPD-09** Portable zip build: press Download → the release page opens in the browser and nothing is downloaded or installed by the app.
- [ ] **UPD-10** Link in the notification opens the default browser at the release page.

## 14. Performance (spec §14)

Run on a release build on the reference machine, otherwise idle, plugged in, High performance power mode, no screen recording. Take the numbers from `cargo xtask perf` (automated proxy) **and** the manual measurements below. Attach the CSV/ETL files to the release issue.

- [ ] **PERF-01** (PresentMon setup) Release build, 60 000-line document (`target/perf/fixtures`) open in a **visible** window on the primary display, nothing else heavy running. Start PresentMon (Intel PresentMon GUI, or `PresentMon.exe --process_name SSWriter.exe --output_file ssw.csv`) → capture starts and lists `SSWriter.exe` as a swap-chain/DComp presenter. If it does not list it, record the present mode used.
- [ ] **PERF-02** (Typing) With PresentMon capturing, type a steady stream of characters for 20 s (physical keyboard, ~10 chars/s or an autotyper outside the app that is not part of the repo) → in the CSV `msBetweenPresents` shows no repeated values above 1.5 × the refresh interval; with PresentMon 2.x input capture (keyboard) `msInputLatency`/`msUntilDisplayed` give input-to-photon ≤ **2 refresh intervals** (16.7 ms × 2 = 33.3 ms at 60 Hz; 16.7 ms at 120 Hz). Record median/p95/max.
- [ ] **PERF-03** (Wheel scrolling) Scroll the whole document with a mouse wheel for 20 s → `msBetweenPresents` median equals the refresh interval and p95 ≤ 1.5 × the interval; no dropped frames visible by eye; the number of presents matches the display refresh rate (60/120/144 Hz).
- [ ] **PERF-04** (Touchpad / touch inertia) Scroll with a precision touchpad (two-finger flick) for 20 s, and with a touch screen (flick) → same thresholds as PERF-03, inertia smooth, CPU/GPU usage low (Task Manager < 15 % one core); DirectManipulation inertia is **not** covered by `cargo xtask perf`, so this is the only measure.
- [ ] **PERF-05** (Window frame, ETW) `wpr -start GeneralProfile`, launch `SSWriter.exe`, wait for the window, `wpr -stop ssw.etl`; in Windows Performance Analyzer read the first DWM present (Microsoft-Windows-Dwm-Core) of the SS Writer window relative to process start → **< 150 ms** with the Mica backdrop visible. Repeat 5 times cold (after reboot) and warm; report the median and the worst. Compare with `window_frame_ms` from `cargo xtask perf`.
- [ ] **PERF-06** (First content frame) Same ETW trace: first frame with document content presented → within the 205-370 ms range (bounded by Direct3D device creation); record the value per run; check that it is not slower than the previous release by more than 10 %.
- [ ] **PERF-07** (60 000-line open) Open the 60 000-line document in the running app by double-clicking in Explorer (single-instance forwarding) and via File → Open → time from the action to the content being displayed by screen recording at 60 fps (e.g. Xbox Game Bar or OBS, one frame ≈ 16.7 ms; counted frames × 16.7 ms) → **< 300 ms** (plus dialog time not counted); UI not frozen during the open.
- [ ] **PERF-08** (10 000-file folder) Open the 10 000-file fixture folder as a workspace → the tree is visible < 300 ms; the index builds in the background without a UI freeze (typing and scrolling stay smooth while indexing; Quick Open becomes complete after indexing).
- [ ] **PERF-09** (Memory/CPU idle) With the 60 000-line document open and idle for 5 minutes → CPU near 0 % (no continuous redraw: the caret blink repaints only the caret region; Task Manager GPU engine near 0 %), private working set within budget (record MB), no growth after 30 min of editing/scrolling (memory leak check), handle count stable.
- [ ] **PERF-10** (Battery) On a laptop on battery, 10 minutes of editing → no unusual power use (Task Manager "Power usage" low); battery saver mode keeps the app responsive (Mica falls back as in WIN-02).
- [ ] **PERF-11** (Multi-monitor / high refresh) Repeat PERF-03 on a 144 Hz monitor and on a secondary monitor with a different refresh rate, then drag the window between them while scrolling → pacing follows the monitor the window is on, no tearing.
- [ ] **PERF-12** (Slow hardware) Run on the oldest supported hardware available (integrated GPU, 4 GB RAM) → editing 60 000-line document remains usable (record numbers; no crash).
- [ ] **PERF-13** Record the final `cargo xtask perf` table in `docs/performance.md` ("Results") and note the machine; confirm manual numbers agree with the automated proxy to within the known limits (the proxy stops at `Present`, not at the photon).

## 15. Accessibility (spec §12 UI Automation, §1 high contrast)

### 15.1 Narrator and UI Automation

- [ ] **A11Y-01** Start Narrator (Win+Ctrl+Enter) and launch the app → the window title is announced ("SS Writer"); Tab/F6 moves through the landmarks Title bar, Tabs ("Open documents"), Commands, Files (sidebar), Editor, Inspector and Status bar with each item announced with name, role and state (selected, expanded, checked, unsaved changes).
- [ ] **A11Y-02** Editor as Document with TextPattern: with Narrator on, arrow keys read characters/words/lines (Caps Lock+arrows read), Ctrl+Left/Right read words, selection changes are announced ("selected …"), the Narrator "read from cursor" (Caps Lock+R) reads the paragraph; hidden Markdown markers are not read as noise (or are read as per design) and headings/links/lists/tasks/tables expose roles (heading level, link, list item, checkbox with state, table cell with row/column headers).
- [ ] **A11Y-03** Typing in the editor with Narrator "Echo characters/words" on → each typed character/word is announced once; the IME candidate window and dictation text are announced by Narrator.
- [ ] **A11Y-04** Sidebar tree is announced as Tree/TreeItem with name, expanded/collapsed, level, position (n of m); Enter/Space/arrow keys work; the cloud (online-only) state is announced.
- [ ] **A11Y-05** Tabs announced as Tab/TabItem with name, selected state and "Unsaved changes"; context menu items, native popup menus, buttons ("Text formatting"/"Text style" B I U pill, "Formatting" selection toolbar), Find bar ("Find and replace": edit, Match case / Whole word / Regular expression toggles, Previous/Next match, `n/m` count), Quick Open list ("Results"), Settings controls (toggle, combo, slider, edit, "Settings pages" navigation) all expose proper names/roles/values; no control is announced as "unknown", "pane" without a name or "button" without a label.
- [ ] **A11Y-06** Dialogs and banners: banners (read-only, conflict, remote image, import report) are announced when they appear (live region/alert) and their actions are keyboard reachable; message boxes read their text.
- [ ] **A11Y-07** Walk the full UIA tree with Accessibility Insights for Windows ("FastPass" and the Tab-stops/Inspect modes) and Inspect.exe → no missing names, no keyboard traps, correct control types, bounding rectangles match the pixels at 100 % and 200 % DPI, focus order is logical, no duplicated nodes.
- [ ] **A11Y-08** Windows Magnifier (Win++): follows the keyboard focus and the text insertion point in the editor, Find bar and Settings; colors invert correctly.
- [ ] **A11Y-09** Voice Access ("click" commands, "show numbers" and dictation into the editor) can invoke menus, tabs and buttons by their names.
- [ ] **A11Y-10** Text scaling: Settings → Accessibility → Text size at 200 % (and the zoom of the app) → UI text scales without clipping where designed; app zoom (Ctrl+=) scales the editor content.
- [ ] **A11Y-11** High contrast and color: see WIN-15 and ICO-03; additionally contrast ratio of dimmed markers, muted completed tasks and the `marker` color meets 4.5:1 for text (check with Accessibility Insights color contrast) in light and dark themes; focus rectangles are visible against every background; information is never conveyed by color only (callout types have icon + title, errors have text).
- [ ] **A11Y-12** Keyboard-only operation: complete the full workflow (open folder → open file → edit → format → find/replace → export → close) without the mouse, including the tab row, sidebar context menu (Shift+F10 / Menu key), Settings and dialogs; focus never gets lost after closing an overlay or tab.
- [ ] **A11Y-13** Reduce motion: with animation effects off all animations stop (smooth scroll, fade, flash, pill transitions), content still reaches its final state immediately.
- [ ] **A11Y-14** Screen reader + IME/dictation interplay: Narrator reads composition/ candidate changes without double speech; the hidden system caret (`CreateCaret`/`SetCaretPos`) lets Narrator's cursor follow the drawn caret in the editor.
- [ ] **A11Y-15** JAWS/NVDA smoke test (if available): open a document, navigate by line/word, read a heading and a list, open a menu, confirm the same roles/names are exposed.

---

## Result summary (fill in at the end)

| Section | Items | Pass | Fail | Skipped |
|---|---|---|---|---|
| 1 Window | | | | |
| 2 Editor | | | | |
| 3 Markdown elements | | | | |
| 4 Files | | | | |
| 5 Workspace | | | | |
| 6 Tabs, windows, session | | | | |
| 7 Search | | | | |
| 8 Inspector and status bar | | | | |
| 9 Copy, share, export, print | | | | |
| 10 Settings | | | | |
| 11 Keyboard | | | | |
| 12 Windows integration | | | | |
| 13 Distribution and updates | | | | |
| 14 Performance | | | | |
| 15 Accessibility | | | | |

Open failures and their issue numbers:

Release sign-off (name / date):
