# Architecture

Behavior lives in [`spec.md`](spec.md). This file fixes structure, contracts and engineering rules.

## Crates

```
crates/
  ssw-core      platform-neutral: text buffer, Markdown AST + parsers, editing, undo, search, stats, TOC,
                wikilinks, commands, keymap model, settings model, i18n (Fluent)
  ssw-io        files on Windows: encodings, atomic save, watcher, workspace tree, SQLite store
                (session, history, index, folds, caret), folder search, recovery, update check, remote images
  ssw-convert   AST → HTML / ePub / DOCX / ODT / RTF / TXT / Markdown / TextBundle;
                DOCX / ODT / RTF / DOC / HTML → Markdown; OMML → LaTeX
  ssw-render    Typst world; Markdown → Typst (PDF/PNG/JPG/print pages); math (MiTeX → Typst frames);
                Mermaid (mermaid-rs-renderer → usvg); `Scene` vector display list
  ssw-ui        platform-neutral UI logic: geometry, theme tokens, Canvas/TextLayout traits, input events,
                widgets + AccessNode tree, editor view model (layout, live render, hit testing,
                virtualization), shell view models (tabs, sidebar, inspector, find, quick open, palette, settings)
  ssw-win       Win32 platform: window/chrome, DComp + D2D Canvas, DirectWrite TextLayout, TSF, UIA bridge,
                DirectManipulation, WIC, clipboard, OLE drag-drop, shell integration, print, spell checker
  ssw-app       binary `SSWriter.exe`: wiring + main loop
  ssw-preview   cdylib Explorer preview handler
third_party/    vendored + patched upstream crates (mermaid-rs-renderer, mitex-spec) with their licenses
xtask/          cargo xtask: coverage, mutants, screenshots, docs sync check, icons, dist (MSI/zip/winget)
assets/         i18n/<locale>/*.ftl, welcome/<locale>.md, fonts/CascadiaMono.ttf (OFL),
                icons/ (app.svg + document.svg masters, committed app.ico + document.ico from `cargo xtask icons`;
                ssw-app's build.rs links them as RT_GROUP_ICON 1 = app, 2 = document, plus VERSIONINFO)
tests/fixtures/ third-party sample documents with tests/fixtures/SOURCES.md (origin + license per file)
installer/      WiX sources; packaging/winget/ manifests
```

Dependency direction (no cycles): `core` ← `io`, `convert`, `render`, `ui` ← `win` ← `app`; `preview` uses `core`, `render`, `ui`, `win`.
Only `ssw-io`, `ssw-win`, `ssw-app`, `ssw-preview` may depend on `windows`. `unsafe` only in `ssw-io`, `ssw-win`, `ssw-preview`; every `unsafe` block has a `// SAFETY:` comment.

## Fixed contracts

- **Offsets**: every text position in core APIs is a UTF-8 byte offset into the document (`usize`), always on a char boundary. Ranges are `TextRange { start, end }` (half-open). UTF-16 conversion happens only at the DirectWrite/TSF boundary via `ssw_core::text::Utf16Map`, cached per block. Rope char indices never leave `ssw_core::text`.
- **AST**: `ssw_core::markdown::ast` is the single tree used by editor, exporters and importers (importers produce Markdown text, then parse). Every node carries its source range and the ranges of its syntax markers.
- **Parsers**: `parse_document(&str) -> Document` is the reference parser. The incremental parser implements the same output for any edit sequence; equivalence is enforced by property tests. Document-level `Definitions` (link reference + footnote definitions) invalidate dependent blocks when they change.
- **Commands**: `ssw_core::command::CommandId` enumerates every user command; menus, keymap, palette, shortcuts docs and UIA names derive from it.
- **Drawing**: `ssw_ui::canvas::Canvas` (fills, strokes, rounded rects, paths, text layouts, glyph runs, images, clips, layers) and `ssw_ui::text::TextLayoutEngine` (styled layout, metrics, hit testing). `ssw-win` implements them with D2D/DirectWrite; tests use `RecordingCanvas` and `MonoTextEngine` (fixed advance) from `ssw-ui`.
- **Vector scene**: `ssw_render::scene::Scene` is a platform-neutral display list (paths, fills, strokes, glyph runs with embedded font bytes, images, groups with transforms). Math, Mermaid, SVG images and print pages produce `Scene`; `ssw-win` draws it with D2D (fonts via DirectWrite in-memory loader). Never rasterize for screen or print.
- **Accessibility**: every widget exposes an `ssw_ui::access::AccessNode` (role, name, bounds, state, patterns). `ssw-win` maps the tree to UIA providers. No widget ships without its node.
- **OS seams**: OS calls that can fail at runtime are behind traits (`ssw_io::fs::FileSystem`, clipboard, dialogs, shell, network) with a real implementation and a fault-injecting test implementation. Fault injection exercises real error-handling code; it never replaces the behavior under test.

## Platform decisions (from spikes, Oct 2026)

Prototype code and full reports live outside the repo in `ssw-spikes/{a-window,b-tsf,c-typst,d-mermaid,e-docx,f-dmanip,g-doc}` on the build machine; the decisions below are binding.

- **Window (a)**: `WM_NCCALCSIZE` custom caption + `DWMSBT_TABBEDWINDOW` + `DWMWA_USE_IMMERSIVE_DARK_MODE`; D3D11 → `CreateSwapChainForComposition` (flip, `DXGI_ALPHA_MODE_PREMULTIPLIED`, `DXGI_SCALING_STRETCH` — `SCALING_NONE` fails with composition swap chains) → DComp visual → D2D device context. Resize: `ResizeBuffers` in `WM_SIZE` + immediate draw/present; `DwmFlush` costs ~6.7 ms/frame and is not used. Device-lost recreate measured at 261 ms. `windows-numerics` is required for D2D geometry types.
- **Startup**: `D3D11CreateDevice` alone takes 205–370 ms (RTX 3080). The device is created on a worker thread at process entry, in parallel with window creation, settings and file load; the window is shown immediately (DWM paints the Mica backdrop), content appears on the first present.
- **Text input (b)**: `ITextStoreACP2` + `ITfContextOwnerCompositionSink` via `#[implement]` against real msctf works; a view-change notification path must not re-enter composition refresh (livelock found and fixed in the spike). Turkish Q/F characters verified through `ToUnicodeEx` + TSF insertion; real IME composition, dictation and emoji panel are manual checks.
- **Typst (c)**: Typst 0.15.1 + `typst-layout` + `typst-pdf` + `typst-render`; MiTeX used as a Rust crate (not the wasm plugin); its Typst spec needs one patch (`sym.kai` removed in 0.15) — vendored under `third_party/mitex-spec`. `\xrightarrow` (xarrow) is unsupported until ported. Fonts bundled: NewCMMath-Regular, NewCM10 Regular/Italic/Bold (3.4 MB); code uses bundled Cascadia Mono. Frame → D2D draws glyphs via `GetGlyphRunOutline` + `FillGeometry` (pixel-equivalent to typst-render at 1×/3×). Cost: the exe is ~40 MB; first formula 13 ms, later 0.3–0.6 ms; 10-page PDF 3.2 ms.
- **Mermaid (d)**: `mermaid-rs-renderer` 0.3.1 (no `cli` feature) vendored under `third_party/mermaid-rs-renderer` (MIT) with local gantt, sequence and flowchart fixes (see its `FIXES.md`); `%%{init}%%`/front matter applied by our wrapper; font Segoe UI; theme palettes for light/dark. `sankey-beta` and `block` (and any render failure, panic or > 500 ms) fall back to the source code block with an "Unsupported diagram" note. SVG → D2D via `usvg` (~400 lines, pixel-equivalent to resvg); +5.6 MB.
- **DOCX (e)**: own OOXML writer (zip + quick-xml); `docx-rs` rejected (no header-row repeat, no core title, no alt text, schema child-order violations). Output validated against the ECMA-376 transitional XSD and rendered with LibreOffice in tests where available.
- **Scrolling (f)**: DirectManipulation in manual-update mode (our own `Update` pump on the frame clock); auto mode stalled off-screen. Wheel: animated, high-resolution deltas applied directly. Real touch/touchpad behaviour is a manual check (no digitizer on the build machine).
- **DOC (g)**: own MS-DOC reader (cfb + encoding_rs): Word 97–2003 text, headings, nested lists, tables, footnotes/endnotes, hyperlinks, bold/italic/strike, embedded raster images. Encrypted files, Word 6/95/2.0 and corrupt containers are rejected with a clear message (non-OLE files are sniffed and routed to the right importer). Every import produces a loss report. Spike: 133/159 Apache POI samples converted, 0 panics; 11/11 docx twins match all body words and headings.

## Threading

- All windows run on one UI thread (documents are shared between tabs/windows).
- Workers: a pool for IO, parsing large files, image decode, math/Mermaid scenes, indexing, search, export. Results post back via `PostMessageW(WM_APP + n)` + channel.
- Every async result carries a generation number (document version / request id); stale results are dropped. Closing a window cancels its jobs.
- `panic = "unwind"`; worker panics are caught per job and reported as job errors.

## Storage

- `%LOCALAPPDATA%\SS Writer\state.db` (SQLite, WAL): session, recent files, per-file caret/scroll/folds, workspace index, backlinks, version history, image cache metadata.
- `%APPDATA%\SS Writer\settings.json`: user settings (unknown keys preserved).
- Portable mode: a file named `portable` next to the exe moves both next to the exe (`data\`).

## Engineering rules

- Edition 2024, toolchain pinned in `rust-toolchain.toml`. `cargo fmt`, `cargo clippy -D warnings` clean.
- Errors: `thiserror` enums per crate; no `unwrap`/`expect` outside tests except on invariants proven by types (with a comment).
- No dead code, no stubs, no `todo!()`/`unimplemented!()` in committed code.
- Release profile: `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`, `strip = true`, `panic = "unwind"`.

## Testing

- Gate: `cargo xtask coverage` → `cargo llvm-cov nextest --workspace` with `--fail-under-lines 100 --fail-under-regions 100 --fail-under-functions 100`. No exclusions: no `#[coverage(off)]`, no ignore-filename regex beyond `tests/` and `xtask/`. Branch coverage is reported on nightly, not gated.
- `cargo xtask mutants` runs `cargo-mutants`; every surviving mutant gets a real test or a written justification in the PR.
- Tests assert observable behavior: document text, file bytes on disk, pixels drawn, AST shape, emitted events. No tests of wiring, mock echoes, or source text.
- Platform tests create real hidden windows, real D2D/DirectWrite objects, WIC bitmap render targets for pixel checks, real TSF/UIA objects in-process. The binary is exercised by tests that launch `SSWriter.exe` with test flags.
- Third-party fixtures only (no self-generated files for importer tests); provenance and license in `tests/fixtures/SOURCES.md`.
- Things only a human can verify (dictation, pen, touchpad feel, SmartScreen) are listed in `docs/manual-test.md`.
