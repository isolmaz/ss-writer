//! Performance instrumentation behind `--perf-report` (spec §14, `docs/performance.md`).
//!
//! A [`Recorder`] collects named marks, per-frame paint timings and input-to-present samples.
//! Every time is in milliseconds since the process was created: the recorder anchors the
//! performance counter to `GetProcessTimes` (creation time) read against
//! `GetSystemTimePreciseAsFileTime`, so a mark placed late in startup is still relative to the
//! process start. The OS stores the creation time at clock-tick resolution (about 1 to 16 ms),
//! which bounds the *absolute* accuracy; differences between two marks are exact to a counter
//! tick.
//!
//! The process-wide recorder exists only after [`enable`]; every free function in this module is
//! a no-op (one atomic load) before that, so the instrumentation is always compiled and costs
//! nothing in a normal run.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::services::clock::{counter, frequency};

/// The window was created (`CreateWindowExW` returned).
pub const MARK_WINDOW_CREATED: &str = "window_created";
/// The Mica backdrop attribute was applied to the window.
pub const MARK_BACKDROP_APPLIED: &str = "backdrop_applied";
/// The Direct3D device was created (on the device worker thread).
pub const MARK_DEVICE_READY: &str = "d3d_device_ready";
/// The first `Present` returned.
pub const MARK_FIRST_PRESENT: &str = "first_present";
/// The first frame that drew a document (an active tab's editor) finished drawing.
pub const MARK_FIRST_CONTENT_PAINTED: &str = "first_content_painted";
/// The first `Present` after [`MARK_FIRST_CONTENT_PAINTED`] returned.
pub const MARK_FIRST_CONTENT_PRESENT: &str = "first_content_present";

/// The stages of a frame that the code drawing it times with [`stage`]; their spans are
/// summed per frame (see [`FrameSample::layout_ms`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The layout pass of the whole view: every widget's arrange, the editor's lazy layout of
    /// the newly visible blocks included.
    Layout,
    /// The editor's own layout (measuring the blocks that came into view); part of
    /// [`Stage::Layout`].
    EditorLayout,
    /// Painting the editor (the draw calls of the visible blocks).
    EditorPaint,
}

impl Stage {
    /// Number of stages.
    const COUNT: usize = 3;

    fn index(self) -> usize {
        match self {
            Stage::Layout => 0,
            Stage::EditorLayout => 1,
            Stage::EditorPaint => 2,
        }
    }
}

/// The moments of one frame, in ms since process start (see [`Recorder::frame_timed`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameTimes {
    /// The frame began: before the back buffer was wrapped.
    pub start_ms: f64,
    /// The back buffer is wrapped, `BeginDraw` ran and the target is cleared: the drawing code
    /// starts.
    pub ready_ms: f64,
    /// The drawing code returned: every draw call was issued (layout included).
    pub recorded_ms: f64,
    /// `EndDraw` returned: the draw calls are flushed to the GPU.
    pub drawn_ms: f64,
    /// `Present` returned.
    pub presented_ms: f64,
}

/// One presented frame (times in ms since process start).
///
/// A frame splits into: set-up (`start_ms` to `ready_ms`: wrap the back buffer, `BeginDraw`,
/// clear), the drawing code (`ready_ms` to `recorded_ms`, of which `layout_ms` is layout and
/// `editor_paint_ms` the editor's draw calls), `EndDraw` (`recorded_ms` to `drawn_ms`) and
/// `Present` (`drawn_ms` to `presented_ms`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSample {
    /// The phase set with [`Recorder::set_phase`] when the frame was presented.
    pub phase: u8,
    /// The frame began: before the back buffer was wrapped.
    pub start_ms: f64,
    /// The drawing code starts (back buffer wrapped, `BeginDraw` and clear done).
    pub ready_ms: f64,
    /// The drawing code returned: every draw call was issued.
    pub recorded_ms: f64,
    /// `EndDraw` returned: layout, drawing and GPU submission are done.
    pub drawn_ms: f64,
    /// `Present` returned.
    pub presented_ms: f64,
    /// Time inside [`Stage::Layout`] during the drawing code (0 when nothing timed it).
    pub layout_ms: f64,
    /// Time inside [`Stage::EditorLayout`] (0 when nothing timed it).
    pub editor_layout_ms: f64,
    /// Time inside [`Stage::EditorPaint`] (0 when nothing timed it).
    pub editor_paint_ms: f64,
}

/// A moment of a [`FrameSample`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameEdge {
    /// [`FrameSample::start_ms`].
    Start,
    /// [`FrameSample::drawn_ms`].
    Drawn,
    /// [`FrameSample::presented_ms`].
    Presented,
}

/// What a [`Recorder`] collected so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Counter ticks per second.
    pub frequency: i64,
    /// Process age (ms) at the moment the recorder started.
    pub age_at_start_ms: f64,
    /// Named marks in the order they were first recorded (ms since process start).
    pub marks: Vec<(&'static str, f64)>,
    /// Every presented frame in order.
    pub frames: Vec<FrameSample>,
    /// For each input that was followed by a frame: ms from the input message to the return of
    /// that frame's `Present`.
    pub input_latency_ms: Vec<f64>,
}

#[derive(Debug, Default)]
struct State {
    marks: Vec<(&'static str, f64)>,
    frames: Vec<FrameSample>,
    latencies: Vec<f64>,
    pending_input: Option<f64>,
    content_pending: bool,
    phase: u8,
    /// Time of the stages of the frame being drawn, indexed by [`Stage::index`].
    stages: [f64; Stage::COUNT],
}

/// Records `ms` under `name` unless the name exists; returns whether it was recorded.
fn record_once(state: &mut State, name: &'static str, ms: f64) -> bool {
    if state.marks.iter().any(|(existing, _)| *existing == name) {
        return false;
    }
    state.marks.push((name, ms));
    true
}

/// Records `ms` under `name`, replacing an earlier value.
fn record_latest(state: &mut State, name: &'static str, ms: f64) {
    match state
        .marks
        .iter_mut()
        .find(|(existing, _)| *existing == name)
    {
        Some(slot) => slot.1 = ms,
        None => state.marks.push((name, ms)),
    }
}

/// Marks, frames and input samples of one process.
#[derive(Debug)]
pub struct Recorder {
    frequency: i64,
    origin: i64,
    age_at_start_ms: f64,
    state: Mutex<State>,
}

/// 100 ns ticks of a `FILETIME`.
fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// Milliseconds between two `FILETIME` tick counts (0 when `created` is in the future).
fn age_ms(now: u64, created: u64) -> f64 {
    now.saturating_sub(created) as f64 / 10_000.0
}

/// Milliseconds since this process was created.
fn process_age_ms() -> f64 {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the pseudo handle of the current process is always valid and the four out
    // pointers reference live `FILETIME`s.
    let times = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    // SAFETY: no arguments; the call only reads the system clock.
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    times.map_or(0.0, |()| {
        age_ms(filetime_ticks(now), filetime_ticks(created))
    })
}

impl Recorder {
    /// A recorder whose time zero is the creation of this process.
    #[must_use]
    pub fn start() -> Self {
        let origin = counter();
        Self::with_anchor(frequency(), origin, process_age_ms())
    }

    /// A recorder that counts `age_at_start_ms` at counter value `origin` (ticks at
    /// `frequency` Hz).
    #[must_use]
    pub fn with_anchor(frequency: i64, origin: i64, age_at_start_ms: f64) -> Self {
        Self {
            frequency: frequency.max(1),
            origin,
            age_at_start_ms,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing panics while the lock is held: a poisoned lock still holds consistent data.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Milliseconds since process start at counter value `ticks`.
    #[must_use]
    pub fn ms_at(&self, ticks: i64) -> f64 {
        (ticks - self.origin) as f64 * 1000.0 / self.frequency as f64 + self.age_at_start_ms
    }

    /// Milliseconds since process start now.
    #[must_use]
    pub fn now_ms(&self) -> f64 {
        self.ms_at(counter())
    }

    /// Records the current time under `name`; a name that exists keeps its first time.
    pub fn mark(&self, name: &'static str) {
        let now = self.now_ms();
        record_once(&mut self.lock(), name, now);
    }

    /// Records the current time under `name`, replacing an earlier time (for events that
    /// repeat: the last one is the one measured).
    pub fn mark_latest(&self, name: &'static str) {
        let now = self.now_ms();
        record_latest(&mut self.lock(), name, now);
    }

    /// Whether `name` was recorded.
    #[must_use]
    pub fn has_mark(&self, name: &'static str) -> bool {
        self.lock()
            .marks
            .iter()
            .any(|(existing, _)| *existing == name)
    }

    /// Records `edge` of frame `index` under `name` (replacing an earlier time). Returns false
    /// when that frame does not exist.
    pub fn mark_frame(&self, name: &'static str, index: usize, edge: FrameEdge) -> bool {
        let mut state = self.lock();
        let Some(frame) = state.frames.get(index).copied() else {
            return false;
        };
        let ms = match edge {
            FrameEdge::Start => frame.start_ms,
            FrameEdge::Drawn => frame.drawn_ms,
            FrameEdge::Presented => frame.presented_ms,
        };
        record_latest(&mut state, name, ms);
        true
    }

    /// Frames are tagged with `phase` from now on.
    pub fn set_phase(&self, phase: u8) {
        self.lock().phase = phase;
    }

    /// Records one presented frame whose drawing code was not timed (set-up and drawing code
    /// count as one span); resolves the pending input sample, if any.
    pub fn frame(&self, start_ms: f64, drawn_ms: f64, presented_ms: f64) {
        self.frame_timed(FrameTimes {
            start_ms,
            ready_ms: start_ms,
            recorded_ms: drawn_ms,
            drawn_ms,
            presented_ms,
        });
    }

    /// Records one presented frame with the moments of its phases and the stage times
    /// collected since [`Recorder::begin_frame`]; resolves the pending input sample, if any.
    pub fn frame_timed(&self, times: FrameTimes) {
        let mut state = self.lock();
        let phase = state.phase;
        let [layout_ms, editor_layout_ms, editor_paint_ms] = std::mem::take(&mut state.stages);
        state.frames.push(FrameSample {
            phase,
            start_ms: times.start_ms,
            ready_ms: times.ready_ms,
            recorded_ms: times.recorded_ms,
            drawn_ms: times.drawn_ms,
            presented_ms: times.presented_ms,
            layout_ms,
            editor_layout_ms,
            editor_paint_ms,
        });
        record_once(&mut state, MARK_FIRST_PRESENT, times.presented_ms);
        if std::mem::take(&mut state.content_pending) {
            record_once(&mut state, MARK_FIRST_CONTENT_PRESENT, times.presented_ms);
        }
        if let Some(received) = state.pending_input.take() {
            state.latencies.push(times.presented_ms - received);
        }
    }

    /// A frame is about to be drawn: the stage times of earlier spans (layout that ran for an
    /// event, not for a frame) are forgotten.
    pub fn begin_frame(&self) {
        self.lock().stages = [0.0; Stage::COUNT];
    }

    /// Adds `ms` to `stage` of the frame being drawn.
    pub fn add_stage(&self, stage: Stage, ms: f64) {
        self.lock().stages[stage.index()] += ms;
    }

    /// A frame finished drawing; `has_content` tells whether it drew document content. The
    /// first such frame arms [`MARK_FIRST_CONTENT_PRESENT`].
    pub fn content_painted(&self, has_content: bool) {
        if !has_content {
            return;
        }
        let now = self.now_ms();
        let mut state = self.lock();
        if record_once(&mut state, MARK_FIRST_CONTENT_PAINTED, now) {
            state.content_pending = true;
        }
    }

    /// An input message arrived; the next presented frame closes the sample. An input that
    /// arrives while another one is waiting for its frame joins that sample (the first one
    /// counts: it waited longest).
    pub fn input_received(&self) {
        let now = self.now_ms();
        let mut state = self.lock();
        if state.pending_input.is_none() {
            state.pending_input = Some(now);
        }
    }

    /// Presented frames so far.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.lock().frames.len()
    }

    /// Closed input samples so far.
    #[must_use]
    pub fn latency_count(&self) -> usize {
        self.lock().latencies.len()
    }

    /// Forgets every mark, frame and input sample and the frame phase; time zero stays the
    /// creation of the process. For a process that measures several runs one after the other
    /// (tests); a normal run records once.
    pub fn reset(&self) {
        *self.lock() = State::default();
    }

    /// Everything recorded so far.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let state = self.lock();
        Snapshot {
            frequency: self.frequency,
            age_at_start_ms: self.age_at_start_ms,
            marks: state.marks.clone(),
            frames: state.frames.clone(),
            input_latency_ms: state.latencies.clone(),
        }
    }
}

// A `OnceLock`, not a `LazyLock`: whether the recorder exists is a runtime decision (the
// `--perf-report` flag), so it must not start itself on first use.
static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// Starts the process-wide recorder (the first call; later calls return it); time zero is the
/// creation of the process.
pub fn enable() -> &'static Recorder {
    RECORDER.get_or_init(Recorder::start)
}

/// The process-wide recorder, once [`enable`] ran.
#[must_use]
pub fn recorder() -> Option<&'static Recorder> {
    RECORDER.get()
}

/// Whether [`enable`] ran.
#[must_use]
pub fn enabled() -> bool {
    RECORDER.get().is_some()
}

/// [`Recorder::reset`] on the process-wide recorder; nothing before [`enable`].
pub fn reset() {
    if let Some(recorder) = RECORDER.get() {
        recorder.reset();
    }
}

/// [`Recorder::mark`] on the process-wide recorder; nothing before [`enable`].
pub fn mark(name: &'static str) {
    if let Some(recorder) = RECORDER.get() {
        recorder.mark(name);
    }
}

/// [`Recorder::mark_latest`] on the process-wide recorder; nothing before [`enable`].
pub fn mark_latest(name: &'static str) {
    if let Some(recorder) = RECORDER.get() {
        recorder.mark_latest(name);
    }
}

/// [`Recorder::input_received`] on the process-wide recorder; nothing before [`enable`].
pub fn input_received() {
    if let Some(recorder) = RECORDER.get() {
        recorder.input_received();
    }
}

/// [`Recorder::content_painted`] on the process-wide recorder; nothing before [`enable`].
pub fn content_painted(has_content: bool) {
    if let Some(recorder) = RECORDER.get() {
        recorder.content_painted(has_content);
    }
}

/// Times one frame: [`FrameTimer::begin`] before the back buffer is wrapped,
/// [`FrameTimer::ready`] when the drawing code is about to run, [`FrameTimer::recorded`] when
/// it returned, [`FrameTimer::drawn`] after `EndDraw`, [`FrameTimer::presented`] after
/// `Present`. A phase that is not marked ends where the previous one did. Inert (no clock
/// reads) when the recorder is not enabled.
#[derive(Debug)]
pub struct FrameTimer {
    recorder: Option<&'static Recorder>,
    start_ms: f64,
    ready_ms: f64,
    recorded_ms: f64,
    drawn_ms: f64,
}

impl FrameTimer {
    /// Starts timing a frame; the stage times of earlier spans are dropped.
    #[must_use]
    pub fn begin() -> Self {
        Self::on(RECORDER.get())
    }

    fn on(recorder: Option<&'static Recorder>) -> Self {
        let start_ms = recorder.map_or(0.0, |recorder| {
            recorder.begin_frame();
            recorder.now_ms()
        });
        Self {
            recorder,
            start_ms,
            ready_ms: start_ms,
            recorded_ms: start_ms,
            drawn_ms: start_ms,
        }
    }

    /// Set-up finished: the drawing code runs next.
    pub fn ready(&mut self) {
        if let Some(recorder) = self.recorder {
            self.ready_ms = recorder.now_ms();
            self.recorded_ms = self.ready_ms;
            self.drawn_ms = self.ready_ms;
        }
    }

    /// The drawing code returned (`EndDraw` is next).
    pub fn recorded(&mut self) {
        if let Some(recorder) = self.recorder {
            self.recorded_ms = recorder.now_ms();
            self.drawn_ms = self.recorded_ms;
        }
    }

    /// Drawing finished (`EndDraw` returned).
    pub fn drawn(&mut self) {
        if let Some(recorder) = self.recorder {
            self.drawn_ms = recorder.now_ms();
        }
    }

    /// `Present` returned: the frame is recorded.
    pub fn presented(self) {
        if let Some(recorder) = self.recorder {
            recorder.frame_timed(FrameTimes {
                start_ms: self.start_ms,
                ready_ms: self.ready_ms,
                recorded_ms: self.recorded_ms,
                drawn_ms: self.drawn_ms,
                presented_ms: recorder.now_ms(),
            });
        }
    }
}

/// Times a [`Stage`] of the frame being drawn: the span from [`stage`] to the drop of this
/// value is added to the stage. Inert (no clock reads) when the recorder is not enabled.
#[derive(Debug)]
#[must_use = "a stage is timed until the value is dropped"]
pub struct StageTimer {
    recorder: Option<&'static Recorder>,
    stage: Stage,
    start_ms: f64,
}

impl StageTimer {
    fn on(recorder: Option<&'static Recorder>, stage: Stage) -> Self {
        Self {
            recorder,
            stage,
            start_ms: recorder.map_or(0.0, Recorder::now_ms),
        }
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        if let Some(recorder) = self.recorder {
            recorder.add_stage(self.stage, recorder.now_ms() - self.start_ms);
        }
    }
}

/// Starts timing `stage` on the process-wide recorder; nothing before [`enable`]. The stage
/// lasts until the returned value is dropped.
pub fn stage(stage: Stage) -> StageTimer {
    StageTimer::on(RECORDER.get(), stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorder with a 1 kHz counter (one tick = one ms) that is `age` ms old at tick 0. It
    /// is for tests that feed times by hand; its clock reads are meaningless.
    fn manual(age: f64) -> Recorder {
        Recorder::with_anchor(1000, 0, age)
    }

    /// A recorder on the real counter that starts at time zero now.
    fn live() -> Recorder {
        Recorder::with_anchor(frequency(), counter(), 0.0)
    }

    #[test]
    fn filetime_ticks_join_both_halves_and_age_never_goes_negative() {
        let time = FILETIME {
            dwLowDateTime: 0x89AB_CDEF,
            dwHighDateTime: 0x0123_4567,
        };
        assert_eq!(filetime_ticks(time), 0x0123_4567_89AB_CDEF);
        assert!((age_ms(30_000, 10_000) - 2.0).abs() < 1e-9);
        assert_eq!(age_ms(5, 10), 0.0);
    }

    #[test]
    fn time_is_counted_from_process_creation() {
        let recorder = manual(40.0);
        assert!((recorder.ms_at(0) - 40.0).abs() < 1e-9);
        assert!((recorder.ms_at(250) - 290.0).abs() < 1e-9);
        // A zero frequency is clamped, never a division by zero.
        let clamped = Recorder::with_anchor(0, 0, 0.0);
        assert!((clamped.ms_at(2) - 2000.0).abs() < 1e-9);
    }

    #[test]
    fn the_real_recorder_starts_at_the_age_of_this_process() {
        let recorder = Recorder::start();
        let snapshot = recorder.snapshot();
        assert!(snapshot.frequency > 0);
        // A test process is alive for a moment, never for ten minutes.
        assert!(snapshot.age_at_start_ms > 0.0 && snapshot.age_at_start_ms < 600_000.0);
        let first = recorder.now_ms();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let second = recorder.now_ms();
        assert!(first >= snapshot.age_at_start_ms);
        assert!(second - first >= 14.0, "{first} {second}");
    }

    #[test]
    fn a_mark_keeps_its_first_time_and_a_latest_mark_its_last() {
        let recorder = live();
        assert!(!recorder.has_mark("a"));
        recorder.mark("a");
        let first = recorder.snapshot().marks[0].1;
        std::thread::sleep(std::time::Duration::from_millis(5));
        recorder.mark("a");
        recorder.mark_latest("b");
        std::thread::sleep(std::time::Duration::from_millis(5));
        recorder.mark_latest("b");
        let marks = recorder.snapshot().marks;
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0], ("a", first));
        assert_eq!(marks[1].0, "b");
        assert!(marks[1].1 > first + 4.0);
        assert!(recorder.has_mark("a") && recorder.has_mark("b"));
    }

    #[test]
    fn a_reset_forgets_everything_and_starts_a_clean_run() {
        let recorder = live();
        recorder.mark("a");
        recorder.set_phase(3);
        recorder.content_painted(true);
        recorder.input_received();
        recorder.frame(1.0, 2.0, 3.0);
        recorder.input_received();
        assert!(recorder.frame_count() == 1 && recorder.latency_count() == 1);
        recorder.reset();
        let snapshot = recorder.snapshot();
        assert!(snapshot.marks.is_empty() && snapshot.frames.is_empty());
        assert!(snapshot.input_latency_ms.is_empty());
        assert_eq!(recorder.frame_count() + recorder.latency_count(), 0);
        // The pending input and the content flag went too: the next frame starts from zero.
        recorder.frame(4.0, 5.0, 6.0);
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.frames[0].phase, 0);
        assert_eq!(snapshot.marks, [(MARK_FIRST_PRESENT, 6.0)]);
        assert!(snapshot.input_latency_ms.is_empty());
    }

    #[test]
    fn frames_carry_their_phase_and_mark_the_first_present() {
        let recorder = manual(0.0);
        assert_eq!(recorder.frame_count(), 0);
        recorder.frame(10.0, 14.0, 15.0);
        recorder.set_phase(2);
        recorder.frame(20.0, 22.0, 33.0);
        let snapshot = recorder.snapshot();
        assert_eq!(recorder.frame_count(), 2);
        assert_eq!(
            snapshot.frames,
            [
                // A frame whose drawing code was not timed: set-up and drawing code are one
                // span, no stage has a time.
                FrameSample {
                    phase: 0,
                    start_ms: 10.0,
                    ready_ms: 10.0,
                    recorded_ms: 14.0,
                    drawn_ms: 14.0,
                    presented_ms: 15.0,
                    layout_ms: 0.0,
                    editor_layout_ms: 0.0,
                    editor_paint_ms: 0.0,
                },
                FrameSample {
                    phase: 2,
                    start_ms: 20.0,
                    ready_ms: 20.0,
                    recorded_ms: 22.0,
                    drawn_ms: 22.0,
                    presented_ms: 33.0,
                    layout_ms: 0.0,
                    editor_layout_ms: 0.0,
                    editor_paint_ms: 0.0,
                },
            ]
        );
        // The second frame did not move the mark.
        assert_eq!(snapshot.marks, [(MARK_FIRST_PRESENT, 15.0)]);
    }

    #[test]
    fn a_frame_keeps_the_moments_of_its_phases_and_the_stage_times_since_it_began() {
        let recorder = manual(0.0);
        // Layout that ran for an event before the frame began does not belong to the frame.
        recorder.add_stage(Stage::Layout, 7.0);
        recorder.begin_frame();
        recorder.add_stage(Stage::Layout, 1.5);
        recorder.add_stage(Stage::Layout, 0.5);
        recorder.add_stage(Stage::EditorLayout, 1.25);
        recorder.add_stage(Stage::EditorPaint, 3.0);
        recorder.frame_timed(FrameTimes {
            start_ms: 10.0,
            ready_ms: 11.0,
            recorded_ms: 18.0,
            drawn_ms: 19.0,
            presented_ms: 20.0,
        });
        // The times were handed to that frame: the next one starts from zero.
        recorder.frame_timed(FrameTimes {
            start_ms: 30.0,
            ready_ms: 31.0,
            recorded_ms: 32.0,
            drawn_ms: 33.0,
            presented_ms: 34.0,
        });
        let frames = recorder.snapshot().frames;
        assert_eq!(
            frames[0],
            FrameSample {
                phase: 0,
                start_ms: 10.0,
                ready_ms: 11.0,
                recorded_ms: 18.0,
                drawn_ms: 19.0,
                presented_ms: 20.0,
                layout_ms: 2.0,
                editor_layout_ms: 1.25,
                editor_paint_ms: 3.0,
            }
        );
        assert_eq!(
            (
                frames[1].layout_ms,
                frames[1].editor_layout_ms,
                frames[1].editor_paint_ms
            ),
            (0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn a_frame_timer_marks_its_phases_in_order_and_a_stage_timer_adds_its_span() {
        // The timers hold a `'static` recorder: this test's own, not the process-wide one.
        static OWN: std::sync::LazyLock<Recorder> = std::sync::LazyLock::new(live);
        let recorder: &'static Recorder = &OWN;
        recorder.add_stage(Stage::EditorPaint, 99.0);
        let mut timer = FrameTimer::on(Some(recorder));
        std::thread::sleep(std::time::Duration::from_millis(2));
        timer.ready();
        {
            let _layout = StageTimer::on(Some(recorder), Stage::Layout);
            let _editor = StageTimer::on(Some(recorder), Stage::EditorLayout);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        timer.recorded();
        std::thread::sleep(std::time::Duration::from_millis(2));
        timer.drawn();
        std::thread::sleep(std::time::Duration::from_millis(2));
        timer.presented();
        let frame = recorder.snapshot().frames[0];
        assert!(frame.ready_ms - frame.start_ms >= 1.5, "{frame:?}");
        assert!(frame.recorded_ms - frame.ready_ms >= 2.5, "{frame:?}");
        assert!(frame.drawn_ms - frame.recorded_ms >= 1.5, "{frame:?}");
        assert!(frame.presented_ms - frame.drawn_ms >= 1.5, "{frame:?}");
        assert!(
            frame.layout_ms >= 2.5 && frame.editor_layout_ms >= 2.5,
            "{frame:?}"
        );
        assert!(frame.layout_ms < 50.0, "{frame:?}");
        // The span before the frame began was dropped, and no one timed the editor's paint.
        assert_eq!(frame.editor_paint_ms, 0.0);
        // A phase that is not marked ends where the one before it did.
        let mut lazy = FrameTimer::on(Some(recorder));
        lazy.recorded();
        lazy.presented();
        let skipped = recorder.snapshot().frames[1];
        assert_eq!(skipped.ready_ms, skipped.start_ms);
        assert_eq!(skipped.drawn_ms, skipped.recorded_ms);
    }

    #[test]
    fn timers_without_a_recorder_do_nothing() {
        let mut timer = FrameTimer::on(None);
        timer.ready();
        timer.recorded();
        timer.drawn();
        timer.presented();
        drop(StageTimer::on(None, Stage::Layout));
    }

    #[test]
    fn a_frame_edge_can_be_marked_after_the_fact() {
        let recorder = manual(0.0);
        recorder.frame(10.0, 14.0, 15.0);
        assert!(recorder.mark_frame("start", 0, FrameEdge::Start));
        assert!(recorder.mark_frame("drawn", 0, FrameEdge::Drawn));
        assert!(recorder.mark_frame("presented", 0, FrameEdge::Presented));
        assert!(!recorder.mark_frame("missing", 1, FrameEdge::Start));
        let marks = recorder.snapshot().marks;
        assert!(marks.contains(&("start", 10.0)));
        assert!(marks.contains(&("drawn", 14.0)));
        assert!(marks.contains(&("presented", 15.0)));
        assert!(!marks.iter().any(|(name, _)| *name == "missing"));
    }

    #[test]
    fn the_first_content_frame_is_marked_when_it_is_presented() {
        let recorder = live();
        // A frame without content arms nothing.
        recorder.content_painted(false);
        recorder.frame(1.0, 2.0, 3.0);
        assert!(!recorder.has_mark(MARK_FIRST_CONTENT_PAINTED));
        assert!(!recorder.has_mark(MARK_FIRST_CONTENT_PRESENT));
        recorder.content_painted(true);
        assert!(recorder.has_mark(MARK_FIRST_CONTENT_PAINTED));
        assert!(!recorder.has_mark(MARK_FIRST_CONTENT_PRESENT));
        recorder.frame(5.0, 6.0, 7.0);
        recorder.content_painted(true);
        recorder.frame(8.0, 9.0, 10.0);
        let marks = recorder.snapshot().marks;
        let present = marks
            .iter()
            .find(|(name, _)| *name == MARK_FIRST_CONTENT_PRESENT)
            .unwrap();
        assert_eq!(present.1, 7.0, "later content frames do not move it");
    }

    #[test]
    fn input_latency_runs_from_the_first_pending_input_to_the_next_present() {
        let recorder = live();
        // A frame without input records no sample.
        recorder.frame(1.0, 2.0, 3.0);
        assert_eq!(recorder.latency_count(), 0);
        recorder.input_received();
        let received = recorder.now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        // A second input before the frame joins the first sample.
        recorder.input_received();
        let presented = recorder.now_ms();
        recorder.frame(received, presented, presented);
        assert_eq!(recorder.latency_count(), 1);
        let latency = recorder.snapshot().input_latency_ms[0];
        assert!((5.0..1000.0).contains(&latency), "{latency}");
        // The sample closed: a frame alone adds none.
        recorder.frame(presented, presented, presented);
        assert_eq!(recorder.latency_count(), 1);
    }

    #[test]
    fn the_process_wide_recorder_is_inert_until_enabled() {
        // The only test of this module that enables the process-wide recorder.
        assert!(!enabled());
        assert!(recorder().is_none());
        mark("early");
        mark_latest("early");
        input_received();
        content_painted(true);
        reset();
        let mut inert = FrameTimer::begin();
        inert.drawn();
        inert.presented();
        drop(stage(Stage::Layout));
        assert!(!enabled());

        let global = enable();
        assert!(std::ptr::eq(global, enable()), "one recorder per process");
        assert!(enabled());
        assert!(std::ptr::eq(global, recorder().unwrap()));
        assert!(global.snapshot().marks.is_empty(), "nothing leaked in");
        mark("late");
        mark_latest("later");
        content_painted(true);
        input_received();
        let mut timer = FrameTimer::begin();
        {
            let _layout = stage(Stage::Layout);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        timer.drawn();
        std::thread::sleep(std::time::Duration::from_millis(3));
        timer.presented();
        let snapshot = global.snapshot();
        let names: Vec<&str> = snapshot.marks.iter().map(|(name, _)| *name).collect();
        for expected in [
            "late",
            "later",
            MARK_FIRST_CONTENT_PAINTED,
            MARK_FIRST_PRESENT,
            MARK_FIRST_CONTENT_PRESENT,
        ] {
            assert!(names.contains(&expected), "{names:?}");
        }
        let frame = snapshot.frames[0];
        assert!(frame.drawn_ms - frame.start_ms >= 2.0);
        assert!(frame.presented_ms - frame.drawn_ms >= 2.0);
        assert!(frame.layout_ms >= 2.0, "{frame:?}");
        assert_eq!(snapshot.input_latency_ms.len(), 1);
        reset();
        assert!(global.snapshot().marks.is_empty() && global.snapshot().frames.is_empty());
    }
}
