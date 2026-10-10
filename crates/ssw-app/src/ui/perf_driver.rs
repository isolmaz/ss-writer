//! The scripted performance scenarios of `--perf-scenario` (spec §14, `docs/performance.md`).
//!
//! A [`PerfDriver`] is ticked from the idle handler of the message loop ([`PerfDriver::drive`]).
//! It waits for the first frame and lets the application settle, then acts through the
//! controller (open a file, open a folder, add an untitled tab) or on the window's own queue
//! (typed characters are *posted* to the window: nothing synthetic reaches the system), waits
//! for the frames those actions cause and records the moments in [`ssw_win::perf`]. When the
//! scenario is over it writes the report and asks the loop to quit.
//!
//! | scenario       | what it does                                                              |
//! |----------------|---------------------------------------------------------------------------|
//! | `startup`      | nothing: the first presented frame ends the run                           |
//! | `open-60k`     | opens the target file in the running app, scrolls the editor through the  |
//! |                | whole document by a stress step per frame, then flicks from the top by a  |
//! |                | realistic step per frame ([`FLICK_STEP`], [`FLICK_FRAMES`] frames)        |
//! | `folder-10k`   | opens the target folder in the sidebar, then builds its workspace index   |
//! | `type-latency` | types characters into an untitled document and times each to `Present`    |
//!
//! Frames are presented for real, so the window must be shown: `--perf-offscreen` puts it where
//! nobody sees it ([`super::host::UiApp::set_offscreen`]).

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_win::perf::{FrameEdge, Recorder};
use ssw_win::services::timers::refresh_interval;
use ssw_win::window::{post_message, quit};
use windows::Win32::UI::WindowsAndMessaging::WM_CHAR;

use super::controller::WindowCell;
use super::host::UiApp;
use crate::args::{Args, PerfScenario};
use crate::perf::{
    DOC_OPEN_BEGIN, DOC_OPEN_RETURNED, DOC_PAINTED, DOC_PRESENTED, FLICK_BEGIN, FLICK_END,
    FOLDER_OPEN_BEGIN, FOLDER_OPEN_RETURNED, INDEX_FINISHED, INDEX_REQUESTED, PHASE_FLICK,
    PHASE_JUMP, PHASE_OPEN, PHASE_SCROLL, PHASE_TYPE, ReportMeta, SCROLL_BEGIN, SCROLL_END,
    TREE_PAINTED, TREE_PRESENTED, TYPE_BEGIN, TYPE_END, build_report, write_report,
};

/// Process exit code of a scenario that failed (the report says why).
pub const EXIT_PERF_FAILED: i32 = 9;

/// DIPs per frame of the scroll sweep unless `--perf-scroll-step` says otherwise: about three
/// quarters of a viewport, so nearly every frame needs content that was not on screen before.
pub const DEFAULT_SCROLL_STEP: u32 = 600;

/// DIPs per frame of the flick sweep that follows: a realistic fast flick (about 6 900 DIPs per
/// second at 144 Hz), a small fraction of the stress step above.
pub const FLICK_STEP: u32 = 48;

/// Frames of the flick sweep (it ends earlier when the document does).
pub const FLICK_FRAMES: u32 = 1200;

/// Characters the typing scenario posts.
pub const TYPED_CHARS: u32 = 120;

/// How long the application settles after its first frame before a scenario acts (startup work
/// on other threads, the first autosave tick).
const SETTLE: Duration = Duration::from_millis(400);

/// A scenario that has not ended by then failed.
const TIMEOUT: Duration = Duration::from_secs(240);

/// The loop wakes at least this often while a scenario runs.
const POLL: Duration = Duration::from_millis(1);

/// How long the loop may sleep once the scenario asked it to quit.
const QUIT_WAIT: Duration = Duration::from_millis(50);

/// Frames of a scroll sweep that has not reached the end of the document are a failure.
const MAX_SCROLL_FRAMES: u32 = 60_000;

/// Pause between two typed characters, so every sample is one character's own frame.
const TYPE_GAP: Duration = Duration::from_millis(30);

/// A typed character that has no frame after this long failed.
const TYPE_TIMEOUT: Duration = Duration::from_secs(3);

/// The characters that get typed, in a loop.
const TYPED_TEXT: &[u8] = b"the quick brown fox jumps over the lazy dog ";

/// How long a scenario may take, in whole and in its parts, before it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// A scenario that has not ended by then failed.
    pub timeout: Duration,
    /// Frames of a scroll sweep that has not reached the end of the document are a failure.
    pub scroll_frames: u32,
    /// A typed character that has no frame after this long failed.
    pub type_timeout: Duration,
    /// Frames of the flick sweep (it ends earlier when the document does).
    pub flick_frames: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: TIMEOUT,
            scroll_frames: MAX_SCROLL_FRAMES,
            type_timeout: TYPE_TIMEOUT,
            flick_frames: FLICK_FRAMES,
        }
    }
}

/// The two scroll sweeps of `open-60k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sweep {
    /// A step of about three quarters of a viewport per frame, to the end of the document: every
    /// frame needs content that was not on screen.
    Stress,
    /// A realistic fast flick from the top of the document: a few dozen DIPs per frame.
    Flick,
}

/// What one tick of the driver decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// The scenario goes on.
    Running,
    /// The scenario ended (report written); the process exit code.
    Finished(i32),
}

/// Where a scenario is.
#[derive(Debug, Clone, Copy)]
enum Stage {
    /// Waiting for the first frame, then for `settled_at`.
    Booting { settled_at: Option<Instant> },
    /// The document is open; waiting for the frame at index `frame`, the first that shows it.
    AwaitDocument { frame: usize },
    /// Scrolling in `sweep`: `requested` is the offset asked of the editor, `requests` how many
    /// were made, `seen` the frame count when it was asked.
    Scrolling {
        sweep: Sweep,
        requested: f32,
        requests: u32,
        seen: usize,
    },
    /// The editor was sent back to the top for the flick sweep; waiting for the frame after the
    /// frame count `seen`.
    AwaitTop { seen: usize },
    /// The folder is open; waiting for the frame at index `frame`, the first that shows it.
    AwaitTree { frame: usize },
    /// The index job runs on its worker.
    Indexing,
    /// The untitled document is added; waiting for the frame at index `frame`.
    AwaitEditor { frame: usize },
    /// `sent` characters posted, `base` input samples existed when typing began, `last` is
    /// when the latest character was posted.
    Typing {
        sent: u32,
        base: usize,
        last: Instant,
    },
}

/// Runs one scenario (or, without a scenario, only records) and writes the report.
#[derive(Debug)]
pub struct PerfDriver {
    recorder: &'static Recorder,
    report: PathBuf,
    scenario: Option<PerfScenario>,
    target: Option<PathBuf>,
    step: f32,
    limits: Limits,
    began: Instant,
    stage: Stage,
    details: Vec<(&'static str, f64)>,
    written: bool,
    finished: Option<i32>,
}

/// The window the scenario works in.
fn window(app: &UiApp) -> Result<Rc<WindowCell>, String> {
    app.controller()
        .active_window()
        .ok_or_else(|| "the application has no window".to_owned())
}

/// Asks for a repaint of `cell` now (the loop would only do so at its next turn).
fn poke(app: &UiApp, cell: &WindowCell) {
    cell.dirty.set(true);
    app.pump();
}

/// The vertical scroll offset of the editor of `cell`.
fn editor_scroll(cell: &WindowCell) -> Result<f32, String> {
    cell.view
        .borrow()
        .active_tab()
        .map(|tab| tab.editor.scroll_y())
        .ok_or_else(|| "the window has no editor".to_owned())
}

impl PerfDriver {
    /// The driver for the `--perf-*` flags of `args`; `None` without `--perf-report`. Starts the
    /// process-wide recorder when it did not run yet.
    #[must_use]
    pub fn new(args: &Args) -> Option<Self> {
        Self::with_limits(args, Limits::default())
    }

    /// [`PerfDriver::new`] with other limits than the ones a real run keeps.
    #[must_use]
    pub fn with_limits(args: &Args, limits: Limits) -> Option<Self> {
        let report = args.test.perf_report.clone()?;
        Some(Self {
            recorder: ssw_win::perf::enable(),
            report,
            scenario: args.test.perf_scenario,
            target: args.test.perf_target.clone(),
            step: args.test.perf_scroll_step.unwrap_or(DEFAULT_SCROLL_STEP) as f32,
            limits,
            began: Instant::now(),
            stage: Stage::Booting { settled_at: None },
            details: Vec::new(),
            written: false,
            finished: None,
        })
    }

    /// The idle step of the message loop: ticks the scenario, keeps the loop awake while it
    /// runs and asks it to quit when it ended. `wait` is what the application itself would
    /// sleep (`None`: it decided to leave).
    pub fn drive(&mut self, app: &UiApp, wait: Option<Duration>) -> Option<Duration> {
        match self.tick(app) {
            Tick::Running => wait.map(|wait| wait.min(POLL)),
            Tick::Finished(code) => {
                quit(code);
                Some(QUIT_WAIT)
            }
        }
    }

    /// One step of the scenario. Writes the report when the scenario ended or failed.
    pub fn tick(&mut self, app: &UiApp) -> Tick {
        if let Some(code) = self.finished {
            return Tick::Finished(code);
        }
        let outcome = if self.began.elapsed() > self.limits.timeout {
            Err(format!(
                "timed out after {} s in {:?}",
                self.limits.timeout.as_secs(),
                self.stage
            ))
        } else {
            self.advance(app)
        };
        let code = match outcome {
            Ok(false) => return Tick::Running,
            Ok(true) => self.conclude(true, None),
            Err(error) => {
                eprintln!("perf scenario failed: {error}");
                self.conclude(false, Some(error))
            }
        };
        self.finished = Some(code);
        Tick::Finished(code)
    }

    /// After the loop ended: writes the report when the scenario did not (a plain
    /// `--perf-report` run, or a window closed before the scenario ended).
    pub fn finish(&mut self) {
        if self.written {
            return;
        }
        let (completed, error) = match self.scenario {
            Some(_) => (
                false,
                Some("the application exited before the scenario finished".to_owned()),
            ),
            None => (true, None),
        };
        if let Err(message) = self.write(completed, error) {
            eprintln!("{message}");
        }
    }

    fn conclude(&mut self, completed: bool, error: Option<String>) -> i32 {
        match self.write(completed, error) {
            Ok(()) if completed => 0,
            Ok(()) => EXIT_PERF_FAILED,
            Err(message) => {
                eprintln!("{message}");
                EXIT_PERF_FAILED
            }
        }
    }

    fn write(&mut self, completed: bool, error: Option<String>) -> Result<(), String> {
        let meta = ReportMeta {
            scenario: self
                .scenario
                .map_or("manual", PerfScenario::name)
                .to_owned(),
            completed,
            error,
            refresh_hz: 1.0 / refresh_interval().as_secs_f64(),
            details: self.details.clone(),
        };
        let report = build_report(&self.recorder.snapshot(), &meta);
        self.written = true;
        write_report(&self.report, &report)
            .map_err(|error| format!("cannot write {}: {error}", self.report.display()))
    }

    fn target(&self) -> Result<PathBuf, String> {
        self.target
            .clone()
            .ok_or_else(|| "the scenario needs --perf-target".to_owned())
    }

    /// Repaints `cell` and waits for the next frame to show the change (`next` is the stage
    /// that waits for it).
    fn await_frame(&mut self, app: &UiApp, cell: &WindowCell, next: impl FnOnce(usize) -> Stage) {
        let frame = self.recorder.frame_count();
        poke(app, cell);
        self.stage = next(frame);
    }

    /// One step of the scenario; `Ok(true)` when it ended.
    fn advance(&mut self, app: &UiApp) -> Result<bool, String> {
        let Some(scenario) = self.scenario else {
            return Ok(false);
        };
        let recorder = self.recorder;
        match self.stage {
            Stage::Booting { settled_at } => {
                if recorder.frame_count() == 0 {
                    return Ok(false);
                }
                if scenario == PerfScenario::Startup {
                    return Ok(true);
                }
                let ready = settled_at.unwrap_or_else(|| Instant::now() + SETTLE);
                self.stage = Stage::Booting {
                    settled_at: Some(ready),
                };
                if Instant::now() < ready {
                    return Ok(false);
                }
                match scenario {
                    PerfScenario::Open60k => self.begin_open(app)?,
                    PerfScenario::Folder10k => self.begin_folder(app)?,
                    // `Startup` returned above.
                    PerfScenario::TypeLatency | PerfScenario::Startup => self.begin_typing(app)?,
                }
                Ok(false)
            }
            Stage::AwaitDocument { frame } => {
                if recorder.frame_count() <= frame {
                    return Ok(false);
                }
                recorder.mark_frame(DOC_PAINTED, frame, FrameEdge::Drawn);
                recorder.mark_frame(DOC_PRESENTED, frame, FrameEdge::Presented);
                let cell = window(app)?;
                let (items, laid_out, viewport) = {
                    let view = cell.view.borrow();
                    let editor = &view
                        .active_tab()
                        .ok_or_else(|| "the window has no editor".to_owned())?
                        .editor;
                    (
                        editor.item_count(),
                        editor.laid_out_items(),
                        editor.scroll_extent().viewport,
                    )
                };
                if laid_out == 0 {
                    return Err("the opened document was not painted".to_owned());
                }
                self.details.push(("editor_items", items as f64));
                self.details.push(("laid_out_items", laid_out as f64));
                self.details.push(("viewport_dip", viewport));
                self.details.push(("scroll_step_dip", f64::from(self.step)));
                recorder.set_phase(PHASE_SCROLL);
                recorder.mark_latest(SCROLL_BEGIN);
                self.scroll_to(app, &cell, Sweep::Stress, self.step, 1);
                Ok(false)
            }
            Stage::Scrolling {
                sweep,
                requested,
                requests,
                seen,
            } => {
                if recorder.frame_count() <= seen {
                    return Ok(false);
                }
                let cell = window(app)?;
                let actual = editor_scroll(&cell)?;
                // The editor clamps a request past the end: the document ended.
                let ended = actual + 0.5 < requested;
                match sweep {
                    Sweep::Stress if ended => {
                        recorder.mark_latest(SCROLL_END);
                        self.details.push(("scroll_frames", f64::from(requests)));
                        self.details
                            .push(("scroll_end_offset_dip", f64::from(actual)));
                        // Back to the top for the flick sweep; the cold frame that takes the
                        // editor there belongs to neither sweep.
                        recorder.set_phase(PHASE_JUMP);
                        let seen = recorder.frame_count();
                        self.set_scroll(app, &cell, 0.0);
                        self.stage = Stage::AwaitTop { seen };
                        Ok(false)
                    }
                    Sweep::Stress if requests >= self.limits.scroll_frames => Err(format!(
                        "the document did not end within {} frames (offset {actual})",
                        self.limits.scroll_frames
                    )),
                    Sweep::Flick if ended || requests >= self.limits.flick_frames => {
                        recorder.mark_latest(FLICK_END);
                        self.details.push(("flick_frames", f64::from(requests)));
                        self.details.push(("flick_step_dip", f64::from(FLICK_STEP)));
                        Ok(true)
                    }
                    sweep => {
                        let step = match sweep {
                            Sweep::Stress => self.step,
                            Sweep::Flick => FLICK_STEP as f32,
                        };
                        self.scroll_to(app, &cell, sweep, actual + step, requests + 1);
                        Ok(false)
                    }
                }
            }
            Stage::AwaitTop { seen } => {
                if recorder.frame_count() <= seen {
                    return Ok(false);
                }
                let cell = window(app)?;
                recorder.set_phase(PHASE_FLICK);
                recorder.mark_latest(FLICK_BEGIN);
                self.scroll_to(app, &cell, Sweep::Flick, FLICK_STEP as f32, 1);
                Ok(false)
            }
            Stage::AwaitTree { frame } => {
                if recorder.frame_count() <= frame {
                    return Ok(false);
                }
                recorder.mark_frame(TREE_PAINTED, frame, FrameEdge::Drawn);
                recorder.mark_frame(TREE_PRESENTED, frame, FrameEdge::Presented);
                // The application starts the index of a shown workspace by itself (the next
                // `pump_files`); the scenario only waits for the job to end.
                self.stage = Stage::Indexing;
                Ok(false)
            }
            Stage::Indexing => Ok(recorder.has_mark(INDEX_FINISHED)),
            Stage::AwaitEditor { frame } => {
                if recorder.frame_count() <= frame {
                    return Ok(false);
                }
                recorder.set_phase(PHASE_TYPE);
                recorder.mark_latest(TYPE_BEGIN);
                self.stage = Stage::Typing {
                    sent: 0,
                    base: recorder.latency_count(),
                    last: Instant::now(),
                };
                Ok(false)
            }
            Stage::Typing { sent, base, last } => {
                // Every posted character must get its own frame before the next one goes.
                if recorder.latency_count() - base < sent as usize {
                    if last.elapsed() > self.limits.type_timeout {
                        return Err(format!("no frame followed typed character {sent}"));
                    }
                    return Ok(false);
                }
                if sent == TYPED_CHARS {
                    recorder.mark_latest(TYPE_END);
                    return Ok(true);
                }
                if last.elapsed() < TYPE_GAP {
                    return Ok(false);
                }
                let hwnd = window(app)?
                    .hwnd
                    .get()
                    .ok_or_else(|| "the window has no handle".to_owned())?;
                let unit = TYPED_TEXT[sent as usize % TYPED_TEXT.len()];
                if !post_message(hwnd, WM_CHAR, usize::from(unit), 1) {
                    return Err(format!("character {sent} could not be posted"));
                }
                self.stage = Stage::Typing {
                    sent: sent + 1,
                    base,
                    last: Instant::now(),
                };
                Ok(false)
            }
        }
    }

    /// `open-60k`: opens the target in the running application.
    fn begin_open(&mut self, app: &UiApp) -> Result<(), String> {
        let recorder = self.recorder;
        let cell = window(app)?;
        let target = self.target()?;
        recorder.set_phase(PHASE_OPEN);
        recorder.mark_latest(DOC_OPEN_BEGIN);
        let opened = app.controller().open_file(&cell, &target);
        recorder.mark_latest(DOC_OPEN_RETURNED);
        opened.map_err(|error| format!("cannot open {}: {error}", target.display()))?;
        let bytes = cell
            .view
            .borrow()
            .active_tab()
            .map_or(0, |tab| tab.entry.document().borrow().buffer().len());
        self.details.push(("doc_bytes", bytes as f64));
        self.await_frame(app, &cell, |frame| Stage::AwaitDocument { frame });
        Ok(())
    }

    /// `folder-10k`: shows the target folder as the sidebar's workspace.
    fn begin_folder(&mut self, app: &UiApp) -> Result<(), String> {
        let recorder = self.recorder;
        let cell = window(app)?;
        let target = self.target()?;
        if !target.is_dir() {
            return Err(format!("{} is not a folder", target.display()));
        }
        recorder.set_phase(PHASE_OPEN);
        recorder.mark_latest(FOLDER_OPEN_BEGIN);
        app.controller().show_workspace(&cell, &target);
        recorder.mark_latest(FOLDER_OPEN_RETURNED);
        // From here on the application indexes the workspace: the wait for the job starts now.
        recorder.mark_latest(INDEX_REQUESTED);
        let rows = cell
            .view
            .borrow()
            .sidebar()
            .map_or(0, |sidebar| sidebar.row_count());
        if rows == 0 {
            return Err("the sidebar shows no rows".to_owned());
        }
        self.details.push(("tree_rows", rows as f64));
        self.await_frame(app, &cell, |frame| Stage::AwaitTree { frame });
        Ok(())
    }

    /// `type-latency`: a fresh untitled document to type into.
    fn begin_typing(&mut self, app: &UiApp) -> Result<(), String> {
        let cell = window(app)?;
        app.controller().new_untitled(&cell);
        self.details.push(("typed_chars", f64::from(TYPED_CHARS)));
        self.await_frame(app, &cell, |frame| Stage::AwaitEditor { frame });
        Ok(())
    }

    /// Asks the editor of `cell` for the offset `y` and repaints. A window without an editor
    /// (its tab was closed since the stage last read it) has nothing to move: the next frame of
    /// the sweep reports that ([`editor_scroll`]).
    fn set_scroll(&self, app: &UiApp, cell: &WindowCell, y: f32) {
        {
            let mut view = cell.view.borrow_mut();
            if let Some(editor) = view.active_editor_mut() {
                editor.set_scroll_y(y);
            }
            view.requests_mut().invalidate_all();
        }
        poke(app, cell);
    }

    /// Asks the editor of `cell` for the offset `y` as request number `requests` of `sweep`,
    /// repaints and waits for the frame.
    fn scroll_to(&mut self, app: &UiApp, cell: &WindowCell, sweep: Sweep, y: f32, requests: u32) {
        let seen = self.recorder.frame_count();
        self.set_scroll(app, cell, y);
        self.stage = Stage::Scrolling {
            sweep,
            requested: y,
            requests,
            seen,
        };
    }
}
