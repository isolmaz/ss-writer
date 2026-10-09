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
xtask/          cargo xtask: coverage, mutants, screenshots, docs sync check, dist (MSI/zip/winget)
assets/         i18n/<locale>/*.ftl, welcome/<locale>.md, fonts/CascadiaMono.ttf (OFL), icons/
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
