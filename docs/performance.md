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

Per frame: start, `EndDraw` returned (layout and drawing), `Present` returned, and the phase
(startup, open, scroll, type). Per typed character: time from the `WM_CHAR` message to the return
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

Not measured yet. Run `cargo xtask perf --write-docs`: it replaces everything between the two
markers of this section with the table of `target/perf/report.md`.

<!-- perf-results:end -->
