# Performance

Targets come from [`spec.md`](spec.md) §14. Measurements are taken by `cargo xtask perf` (this
file's results section) and, for what a process cannot see about itself, by hand with PresentMon
(see "Manual measurement" below, also listed in `docs/manual-test.md`).

## Method

`cargo xtask perf [--runs N] [--write-docs]`:

1. `cargo build --release -p ssw-app`.
2. Fixtures in `target/perf/fixtures` (created once): a 60 000-line Markdown file (headings,
   paragraphs, links, lists, tables, code blocks, quotes) and a folder tree of 10 000 Markdown
   files (10 x 10 folders, 100 files each, with wiki links).
3. Each scenario runs N times (default 5) as a fresh `SSWriter.exe` with a fresh
   `--test-profile` folder, so no state carries over:

   | scenario | flags | what happens |
   |---|---|---|
   | `startup` | `--perf-scenario startup` | no document; ends after the first content frame |
   | `open-60k` | `--perf-scenario open-60k --perf-target <file>` | after the first frame and a 400 ms settle the file is opened in the running app, then the editor is scrolled programmatically through the whole document by a fixed step per frame (`--perf-scroll-step`, default 600 DIP) |
   | `folder-10k` | `--perf-scenario folder-10k --perf-target <folder>` | the folder becomes the sidebar's workspace, then its index is built |
   | `type-latency` | `--perf-scenario type-latency` | 120 characters are *posted* (`PostMessageW`) to the app's own window, one per frame |

   All runs add `--perf-offscreen --perf-report <json>`. The window is placed at
   (-20000, -20000) and shown with `SW_SHOWNOACTIVATE` and `WS_EX_NOACTIVATE`: frames are
   presented for real, nothing appears on a screen, nothing takes the focus, and no input is
   injected. `--perf-scenario` and `--perf-offscreen` refuse to run without `--test-profile`.
4. `target/perf/report.md` lists median and p95 per metric against the targets; raw reports are
   in `target/perf/runs/*.json`. `--write-docs` replaces the results section below.

A plain `SSWriter.exe --test-profile <dir> --perf-report <file>` (no scenario) records an
ordinary session and writes the report when the app exits.

## What is recorded

All times are milliseconds since the creation of the process (`GetProcessTimes` creation time
read against `GetSystemTimePreciseAsFileTime`, then `QueryPerformanceCounter`). The OS stores the
creation time at clock-tick resolution, so the absolute values carry an error of about 1 to 16 ms;
differences between marks are exact. The recorder is always compiled; without `--perf-report` it
is one atomic load per call site.

Marks (`ssw_win::perf`, `ssw_app::perf`): `main_entered`, `startup_done`, `d3d_device_ready`
(device worker thread), `devices_received`, `window_created`, `backdrop_applied` (Mica attribute
set), `graphics_attached`, `window_shown`, `first_present`, `first_content_painted`,
`first_content_present`, `doc_open_begin`, `doc_read`, `doc_decoded`, `doc_parsed`,
`doc_open_returned`, `doc_painted`, `doc_presented`, `scroll_begin`, `scroll_end`,
`folder_open_begin`, `folder_open_returned`, `tree_painted`, `tree_presented`, `index_requested`,
`index_started`, `index_finished`, `type_begin`, `type_end`.

Startup order (production, `UiApp::start` with `DeviceStart::Defer`): the device worker is spawned
first thing in `boot`; the UI thread creates WIC, DirectWrite, the controller and the window while
it runs. The window is created, gets its Mica / dark-mode attributes and is shown **without** a
swap chain (`window_created`, `backdrop_applied`, `window_shown`: DWM draws the backdrop of the
`WS_EX_NOREDIRECTIONBITMAP` window, there is no white fill). When the worker is done it wakes the
loop; `UiApp::pump` takes the devices (`devices_received`), attaches the swap chains
(`graphics_attached`) and invalidates the windows, so `first_present` and `first_content_present`
follow. `window_shown` therefore always precedes `devices_received`; `d3d_device_ready` may fall
before or after `window_shown`, which is what `window_frame_ms` no longer waits for. Tests that
draw at once use `UiApp::new` (waits for the devices, the old order).

Per frame: start, the moment the drawing code starts (`ready`: back buffer wrapped, `BeginDraw`
and clear done), the moment it returned (`recorded`), `EndDraw` returned, `Present` returned, the
time spent in the layout pass of the view, in the editor's lazy layout (inside the layout pass)
and in the editor's draw calls, and the phase (startup, open, scroll, type). The scroll scenario
reports each stage as a median and p95 info metric (`scroll_setup_*`, `scroll_code_*` = layout
plus draw calls, `scroll_layout_*`, `scroll_editor_layout_*`, `scroll_editor_paint_*`,
`scroll_enddraw_*`, `scroll_present_*`), so a slow frame is attributed to layout, drawing, the
GPU flush or `Present` (chrome draw calls are `scroll_code` minus layout and editor paint). Per typed character: time from the `WM_CHAR` message to the return
of the next `Present`. The JSON format is documented in `crates/ssw-app/src/perf.rs`.

## Metrics and how they are judged

A target passes when the **p95 over the runs** (for `type_latency_ms`: over all samples) is within
the limit.

| Metric | Definition | Target (§14) |
|---|---|---|
| `window_frame_ms` | `window_shown`: Mica attribute applied and `ShowWindow` returned | < 150 ms |
| `first_content_frame_ms` | `first_content_present`: `Present` returned for the first frame that drew document content | <= 370 ms (upper end of the 205-370 ms device creation range) |
| `open_ms` | `doc_open_begin` to `doc_presented`: read, decode, parse, layout, draw, present of a 60 000-line file in the running app | < 300 ms |
| `scroll_draw_p95_ms` | p95 of draw time (layout + `EndDraw`) of the scroll frames | <= one refresh interval |
| `scroll_interval_median_ms` | median time between two presented scroll frames | <= 1.1 x refresh interval |
| `scroll_interval_p95_ms` | p95 of that interval | <= 1.5 x refresh interval |
| `type_latency_ms` | `WM_CHAR` receipt to `Present` returned | <= 2 frames |

Reported without target: the startup breakdown (`main_entered_ms`, `startup_done_ms`,
`d3d_device_ready_ms`, `window_created_ms`, `backdrop_applied_ms`, `first_present_ms`), the open
breakdown (`open_read_ms`, `open_decode_parse_ms`, `open_layout_paint_ms`, `open_present_ms`),
`scroll_frames`, `tree_visible_ms` (folder open to the sidebar rows presented), `index_ms` (index
job on its worker) and `index_wait_ms` (workspace shown, which is when the app requests its
index, to job start).

## Limits of this measurement

- `type_latency_ms` is a **proxy**. It stops at the return of `Present`, not at the photon: the
  compositor, the display pipeline and the panel add their own latency (typically one to two
  refresh intervals more). It also starts at message receipt, not at the key switch.
- The scroll step is programmatic (`EditorView::set_scroll_y` per frame), not a wheel or touch
  gesture; DirectManipulation inertia is not exercised.
- `Present(1)` pacing of an off-screen window depends on DWM; the draw-time metric does not.
  The refresh rate reported is the primary display's (`DwmGetCompositionTimingInfo`).
- `window_frame_ms` is the moment the window was shown, not the moment DWM composed the frame.

## Manual measurement (PresentMon)

For the input-to-photon target as the user sees it (belongs in `docs/manual-test.md`):

1. Release build, a 60 000-line document open in a visible window on the primary display, no
   other heavy load. Start PresentMon (Intel PresentMon GUI or `PresentMon.exe --process_name
   SSWriter.exe --output_file ssw.csv`).
2. Type a steady stream of characters for 20 s and scroll the document with the wheel and with a
   precision touchpad for 20 s.
3. In the CSV, `msBetweenPresents` shows the frame pacing (scroll at refresh rate: median at the
   refresh interval, no repeated values above 1.5 x), `msUntilDisplayed` and `msInputLatency`
   (PresentMon 2.x with the click-to-photon or keyboard input capture enabled) give
   input-to-photon, which must be <= 2 refresh intervals.
4. For the window-frame target, record an ETW trace (`wpr -start GeneralProfile`,
   launch, `wpr -stop`) and read the first DWM present of the window in WPA against the process
   start.

## Results

<!-- perf-results:begin -->

Runs per scenario: 5. Display refresh: 144.00 Hz. A target passes when the p95 over the runs (or over all samples for `type_latency_ms`) is within its limit.

### startup

| Metric | Target | Median | p95 | n | Verdict |
|---|---|---:|---:|---:|---|
| window_frame_ms | <= 150.00 (§14: window frame with Mica < 150 ms) | 305.26 | 318.22 | 5 | FAIL |
| first_content_frame_ms | <= 370.00 (§14: bounded by device creation (205-370 ms)) | 305.23 | 318.17 | 5 | pass |
| main_entered_ms | - | 16.27 | 24.80 | 5 | info |
| startup_done_ms | - | 27.84 | 42.56 | 5 | info |
| d3d_device_ready_ms | - | 200.65 | 208.41 | 5 | info |
| window_created_ms | - | 263.72 | 272.60 | 5 | info |
| backdrop_applied_ms | - | 263.76 | 272.64 | 5 | info |
| first_present_ms | - | 305.23 | 318.17 | 5 | info |

### open-60k

| Metric | Target | Median | p95 | n | Verdict |
|---|---|---:|---:|---:|---|
| open_ms | <= 300.00 (§14: open < 300 ms) | 223.35 | 232.24 | 5 | pass |
| open_read_ms | - | 1.12 | 1.16 | 5 | info |
| open_decode_parse_ms | - | 106.48 | 110.28 | 5 | info |
| open_layout_paint_ms | - | 114.39 | 122.47 | 5 | info |
| open_present_ms | - | 0.08 | 0.08 | 5 | info |
| scroll_draw_p95_ms | <= 6.94 (§14: scrolling at refresh rate: draw fits one refresh interval) | 19.14 | 20.87 | 5 | FAIL |
| scroll_interval_median_ms | <= 7.64 (§14: median frame interval <= 1.1 x refresh interval) | 17.29 | 17.52 | 5 | FAIL |
| scroll_interval_p95_ms | <= 10.42 (§14: p95 frame interval <= 1.5 x refresh interval) | 19.32 | 21.07 | 5 | FAIL |
| scroll_frames | - | 2288.00 | 2288.00 | 5 | info |

### folder-10k

| Metric | Target | Median | p95 | n | Verdict |
|---|---|---:|---:|---:|---|
| tree_visible_ms | - | 4.52 | 8.60 | 5 | info |
| index_ms | - | 1653.87 | 1687.31 | 5 | info |
| index_wait_ms | - | 0.37 | 0.46 | 5 | info |

### type-latency

| Metric | Target | Median | p95 | n | Verdict |
|---|---|---:|---:|---:|---|
| type_latency_ms | <= 13.89 (§14: input-to-photon <= 2 frames (proxy: message to Present)) | 5.29 | 8.41 | 600 | pass |
<!-- perf-results:end -->
