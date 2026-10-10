# Contributing to SS Writer

Thank you for helping. SS Writer is a native Windows 11 Markdown and text editor written in Rust, released under the [MIT License](LICENSE); by contributing you agree that your contribution is licensed the same way.

- Found a bug or have an idea? Open an [issue](https://github.com/isolmaz/ss-writer/issues). For a larger change, open an issue first so the approach can be agreed before you write code.
- [`docs/spec.md`](docs/spec.md) is the single source of truth for behavior. Code, tests, the README, [`docs/features.toml`](docs/features.toml) and the website must agree with it, and when behavior changes the spec changes in the same commit.
- [`docs/architecture.md`](docs/architecture.md) fixes the structure, the contracts between crates and the engineering rules. Read its "Fixed contracts", "Engineering rules" and "Testing" sections before your first change.

## Development setup

You need a Windows 11 x64 machine (the app and most of its tests are Windows-only).

1. Install [rustup](https://rustup.rs/) and the Visual Studio Build Tools with the C++ workload. The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (Rust 1.98.0 with rustfmt, clippy and llvm-tools-preview, target `x86_64-pc-windows-msvc`); rustup installs it the first time you run cargo in the checkout. The workspace uses edition 2024.
2. Install the cargo tools the gate uses:

   ```powershell
   cargo install cargo-nextest cargo-llvm-cov
   cargo install cargo-mutants   # only for mutation testing
   ```

3. Only to build the installer: .NET and WiX 7 (`dotnet tool install --global wix --version 7.0.0`).
4. Clone, build and run:

   ```powershell
   git clone https://github.com/isolmaz/ss-writer.git
   cd ss-writer
   cargo build
   cargo run -p ssw-app    # starts SSWriter.exe (debug build)
   ```

`.cargo/config.toml` defines the alias `cargo xtask`, which runs the project automation in [`xtask/`](xtask).

## Workspace layout

```
crates/
  ssw-core      platform-neutral: text buffer, Markdown AST + parsers, editing, undo, search, stats,
                TOC, wikilinks, commands, keymap model, settings model, i18n (Fluent)
  ssw-io        files on Windows: encodings, atomic save, watcher, workspace tree, SQLite store
                (session, history, index, folds, caret), folder search, recovery, update check,
                remote images
  ssw-convert   AST -> HTML / ePub / DOCX / ODT / RTF / TXT / Markdown / TextBundle;
                DOCX / ODT / RTF / DOC / HTML -> Markdown; OMML -> LaTeX
  ssw-render    Typst world; Markdown -> Typst (PDF/PNG/JPG/print pages); math (MiTeX -> Typst
                frames); Mermaid (mermaid-rs-renderer -> usvg); the `Scene` vector display list
  ssw-ui        platform-neutral UI logic: geometry, theme tokens, Canvas/TextLayout traits, input
                events, widgets + AccessNode tree, editor view model, shell view models (tabs,
                sidebar, inspector, find, quick open, palette, settings)
  ssw-win       Win32 platform: window/chrome, DComp + D2D Canvas, DirectWrite TextLayout, TSF, UIA
                bridge, DirectManipulation, WIC, clipboard, OLE drag-drop, shell integration,
                print, spell checker
  ssw-app       binary SSWriter.exe: wiring + main loop
  ssw-preview   cdylib Explorer preview handler
third_party/    vendored + patched upstream crates (mermaid-rs-renderer, mitex-spec) with licenses
xtask/          cargo xtask: coverage, mutants, screenshots, docs sync check, icons, perf, dist
assets/         i18n/<locale>/*.ftl, welcome/<locale>.md, fonts, icons
tests/fixtures/ third-party sample documents; SOURCES.md records origin and license of each file
installer/      WiX sources;  packaging/winget/  winget manifest templates
docs/           spec, architecture, manual test checklist, performance, features.toml, screenshots
```

Dependency direction (no cycles): `core` <- `io`, `convert`, `render`, `ui` <- `win` <- `app`; `preview` uses `core`, `render`, `ui` and `win`.

- **`ssw-ui` depends only on `ssw-core`.** It never touches the OS: drawing and text layout go through the `Canvas` and `TextLayoutEngine` traits, which `ssw-win` implements and the tests replace with `RecordingCanvas` and `MonoTextEngine`.
- Only `ssw-io`, `ssw-win`, `ssw-app` and `ssw-preview` may depend on the `windows` crate.
- `unsafe` is allowed only in `ssw-io`, `ssw-win` and `ssw-preview`, and every `unsafe` block has a `// SAFETY:` comment.
- OS calls that can fail at runtime sit behind traits with a real implementation and a fault-injecting test implementation. Fault injection exercises the real error handling; it never replaces the behavior under test.
- Every text position in `ssw-core` APIs is a UTF-8 byte offset on a char boundary; UTF-16 conversion happens only at the DirectWrite/TSF boundary.

## The gate

A change is ready when all of these pass:

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo xtask docs-check --strict
```

and the coverage gate below is still at 100 %. CI (`.github/workflows/ci.yml`) runs format, clippy, tests under `cargo nextest`, doc tests, the docs check and the coverage gate on `windows-latest` for every push and pull request; mutation testing (`.github/workflows/mutants.yml`) runs there too, by hand or weekly, in eight shards; the tag-triggered release workflow builds the installer and the portable zip. The full suite, the coverage gate and mutation testing are meant to run on GitHub: locally, run the tests of the crate you changed (`cargo nextest run -p <crate> --all-features`). A test that hangs is stopped after three minutes (`.config/nextest.toml`).

Workspace lints are strict: `clippy::all` is denied, `clippy::undocumented_unsafe_blocks` is denied, `unsafe_op_in_unsafe_fn` is denied and every public type implements `Debug`. Rustfmt uses `max_width = 100`; files use LF line endings (`.gitattributes`).

Engineering rules (details in `docs/architecture.md`):

- Errors are `thiserror` enums per crate. No `unwrap` or `expect` outside tests, except on invariants proven by types, with a comment saying why.
- No dead code, no stubs, no `todo!()` or `unimplemented!()` in committed code.
- New public items get doc comments.

### Coverage policy

The workspace is held at **100 % line, region and function coverage**, measured with `cargo llvm-cov`:

```powershell
cargo llvm-cov nextest --workspace --all-features `
  --ignore-filename-regex "(^|[\\/])(tests|xtask|third_party)[\\/]|[\\/]ssw-win[\\/]src[\\/]system_ui\.rs$" `
  --fail-under-lines 100 --fail-under-regions 100 --fail-under-functions 100
```

`cargo xtask coverage` is the project's wrapper around this gate: it runs the instrumented tests, writes `lcov.info` and `target/coverage-summary.txt`, and fails below 100 % lines, regions or functions. Branch coverage is reported on nightly but not gated.

There is **one documented coverage exception**: `crates/ssw-win/src/system_ui.rs`. It holds only the calls that put system UI on the screen and wait for the user (`TrackPopupMenuEx`, `IModalWindow::Show`, `TaskDialogIndirect`, `PrintDlgEx`), because no test may show system UI. Everything around those calls (building menus, dialog configurations and print requests, reading their answers) lives in the covered modules and is tested through hosts that answer as a user would. Apart from that file there are no exclusions: no `#[coverage(off)]`, no `#[cfg(not(coverage))]`, no `#[ignore]` tests, and no ignore pattern beyond `tests/`, `xtask/` and the vendored `third_party/` code. Code that cannot be reached is deleted or restructured (for example by making an impossible error arm impossible through types), not excluded. Error paths of OS calls are reached with real failing inputs (invalid handles, bad paths, malformed data, closed streams) or through the `fault-injection` Cargo feature of `ssw-io`, `ssw-win`, `ssw-convert` and `ssw-preview` (a test-only `fault` module that makes the n-th checked system call fail).

Tests assert observable behavior: document text, bytes on disk, pixels drawn, AST shape, emitted events. Do not write tests of wiring, tests that echo a mock, tests that read source text, or tests whose only assertion is "does not panic". Importer tests use third-party fixtures from `tests/fixtures/` only (add the provenance and license of a new file to `tests/fixtures/SOURCES.md`), never files generated by the code under test.

`cargo xtask mutants` runs `cargo-mutants`; a surviving mutant gets a real test or a written justification in the pull request.

### Test safety rules

Developers run the tests on the machine they work on, so tests must never get in their way:

- Tests **never show system UI**: no message boxes, print or share dialogs, file pickers or Explorer windows. These go through the service traits (`ssw_ui::services`, `ssw_io::fs`, the shell host seam) and tests use the recording or scripted implementations.
- Tests **never steal focus and never inject input**: no `SendInput`, no `SetCursorPos`, no pointer injection, no activating or foregrounding windows. Windows created by tests are hidden and never shown. `cargo xtask screenshots` renders every scenario in a hidden window, and `cargo xtask perf` presents real frames only in a window placed off-screen and shown without activation (`--perf-offscreen`, `WS_EX_NOACTIVATE`).
- Tests **never touch the registry** (no changes to file associations, themes, keyboard layouts or settings) **and never touch the real Recycle Bin**. Deleting, associations and the Recycle Bin are exercised through the seams.
- Tests leave user state untouched: the clipboard, spell-check dictionaries and Jump List are restored; files live in temporary directories; app data goes to a `--test-profile <dir>` or to injected paths, never to the real `%APPDATA%` or `%LOCALAPPDATA%`. A test that launches `SSWriter.exe` passes `--test-hidden` and `--test-profile <temporary directory>`.
- No network except loopback servers the test starts itself.
- `cargo nextest` runs every test in its own process, in parallel. Anything that touches a resource shared by the whole session (clipboard, user spell-check dictionary, Jump List and AppUserModelID, DirectManipulation) must serialize across processes with a named mutex from a shared test-support module (see `crates/ssw-win/tests/support`), not only with an in-process `Mutex`.
- Never kill processes you did not start.

What only a human can verify (dictation, pen and touch feel, IME composition, SmartScreen, real printing) belongs in [`docs/manual-test.md`](docs/manual-test.md) as a checklist item with steps and the expected result.

## User interface strings

Every user-visible string comes from Fluent files in `assets/i18n/en/*.ftl` (English is the source locale; the app falls back to it). There are no string literals in UI code.

- Commands: add the variant to `CommandId` in `crates/ssw-core/src/command.rs` together with its `cmd-<group>-<name>` message in `assets/i18n/en/main.ftl`. Menus, keymap, palette, the shortcuts document and UI Automation names derive from `CommandId`.
- Other strings: put them in the `.ftl` file of your area, with ids prefixed by the area (`editor-…`, `sidebar-…`, `settings-…`). Widgets take a `Localizer` or already resolved strings.
- `cargo xtask docs-check` fails when a message id written as a string literal in the code is not defined in `assets/i18n/en`.

## Documentation that must move with the code

| You change | Also update |
|---|---|
| Any user-visible behavior | `docs/spec.md` (same commit), the matching entry in `docs/features.toml`, `CHANGELOG.md` under `[Unreleased]` |
| A default shortcut | `docs/spec.md` section 11 and the default keymap in `ssw-core` (the check compares them; a new spec row also needs an entry in `ROWS` in `xtask/src/docs_check/shortcuts.rs`) |
| A new feature | A spec section, a `[[feature]]` in `docs/features.toml` (id, title, spec, description), and a line in `README.md` containing the feature's exact title |
| Something only a human can verify | `docs/manual-test.md` |

`cargo xtask docs-check --strict` compares the spec's shortcut table with the default keymap, checks that every feature in `docs/features.toml` points at an existing spec section and every spec section has a feature, that every feature title appears on a README line, and that every Fluent id used in code is defined.

## xtask commands

`cargo xtask <command>` (run `cargo xtask help` for the same list):

| Command | What it does |
|---|---|
| `coverage` | `cargo llvm-cov nextest` over the workspace; fails below 100 % lines, regions and functions; writes `lcov.info` and `target/coverage-summary.txt` |
| `mutants [-j N] [-- ARGS]` | Runs cargo-mutants (N parallel jobs, default 4; `ARGS` go to cargo-mutants) and writes `target/mutants-report.md` |
| `dist [--accept-wix-eula] [--expect-tag vX.Y.Z]` | Release build, then `dist/`: MSI, portable zip, `SHA256SUMS.txt`, `THIRD-PARTY-NOTICES.txt` and winget manifests |
| `docs-check [--strict]` | Spec, keymap, `features.toml`, README and `.ftl` sync check; `--strict` turns warnings (missing README lines) into failures |
| `icons` | Rasterizes `assets/icons/*.svg` into the multi-size `assets/icons/*.ico` files that `crates/ssw-app/build.rs` embeds into `SSWriter.exe` |
| `perf [--runs N] [--write-docs]` | Release build, fixtures in `target/perf`, every performance scenario N times (default 5, off-screen window, fresh profile per run); report in `target/perf/report.md`; `--write-docs` fills `docs/performance.md` too |
| `screenshots [--scale N] [--only NAME]... [--no-build]` | Release build, demo documents and scenario scripts in `target/screens`, every scenario run hidden in the light and dark theme; PNG files in `docs/screenshots`; `--scale 2` for hi-DPI (default 1) |
| `help` | Prints the usage text |

`dist` needs WiX 7 and `--accept-wix-eula` to accept its Open Source Maintenance Fee EULA (<https://wixtoolset.org/osmf/>).

The vendored `third_party/mermaid-rs-renderer` is not a workspace member: it keeps its own lints and test suite (`cargo test --manifest-path third_party/mermaid-rs-renderer/Cargo.toml --lib`); local changes are recorded in its `FIXES.md`.

## Commits and pull requests

- One logical change per commit and per pull request, based on `main`.
- Subject line: imperative mood, sentence case, no trailing period, at most 72 characters ("Retry atomic saves on sharing violations"). A crate or area prefix is welcome when it helps ("ssw-io: …"). Use the body to explain why the change is needed and what was verified.
- The gate above passes on every commit you propose, and the commit includes the tests, spec, features, README and changelog updates that the change needs.
- Do not commit generated output (`target/`, `dist/`, `lcov.info`).
- Describe in the pull request what you ran, and for anything the automated tests cannot see (rendering, input devices, installer behavior) which checklist items of `docs/manual-test.md` you ran.
