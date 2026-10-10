//! The performance report of one run (`--perf-report`, spec §14, `docs/performance.md`): the
//! names of the marks the application places, the frame phases and the JSON the run writes.
//!
//! The recorder itself lives in [`ssw_win::perf`] (marks, frame timings, input latency; it
//! also places the marks of the platform layer). This module only names the application's own
//! marks and turns a [`Snapshot`] into the report `cargo xtask perf` reads.
//!
//! # Report format (`schema` 1)
//!
//! ```text
//! {
//!   "schema": 1, "scenario": "open-60k", "completed": true, "error": null,
//!   "refresh_hz": 60.0, "process_age_at_start_ms": 7.9, "qpc_hz": 10000000,
//!   "details": { "doc_bytes": 3.1e6, ... },          // numbers describing the run
//!   "marks":   { "window_shown": 91.2, ... },        // ms since process creation
//!   "phases":  ["startup", "open", "scroll", "type", "flick", "jump"],
//!   "frames":  { "phase": [0, ...],
//!                "start_ms": [...],        // the frame began (before the back buffer is wrapped)
//!                "ready_ms": [...],        // back buffer wrapped, BeginDraw + clear done
//!                "recorded_ms": [...],     // the drawing code (layout + draw calls) returned
//!                "drawn_ms": [...],        // EndDraw returned
//!                "presented_ms": [...],    // Present returned
//!                "layout_ms": [...],       // durations: layout pass of the view,
//!                "editor_layout_ms": [...],// the editor's lazy layout (inside layout_ms),
//!                "editor_paint_ms": [...]  // the editor's draw calls (inside recorded - ready)
//!              },
//!   "input_latency_ms": [ ... ]                      // WM_CHAR receipt -> Present returned
//! }
//! ```

use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};
use ssw_win::perf::{FrameSample, Snapshot};

/// The version of the report format.
pub const SCHEMA: u32 = 1;

/// `main` started.
pub const MAIN_ENTERED: &str = "main_entered";
/// Startup finished (settings, state database and session loaded; device still being created).
pub const STARTUP_DONE: &str = "startup_done";
/// The graphics device was handed to the UI thread.
pub const DEVICES_RECEIVED: &str = "devices_received";
/// The window has its swap chain and render resources.
pub const GRAPHICS_ATTACHED: &str = "graphics_attached";
/// `ShowWindow` returned for the first window.
pub const WINDOW_SHOWN: &str = "window_shown";
/// The scenario asked to open the document.
pub const DOC_OPEN_BEGIN: &str = "doc_open_begin";
/// The bytes of the document were read.
pub const DOC_READ: &str = "doc_read";
/// The bytes were decoded to text.
pub const DOC_DECODED: &str = "doc_decoded";
/// The text was parsed into a document.
pub const DOC_PARSED: &str = "doc_parsed";
/// The open call returned (tab added).
pub const DOC_OPEN_RETURNED: &str = "doc_open_returned";
/// The first frame that shows the opened document finished drawing (layout included).
pub const DOC_PAINTED: &str = "doc_painted";
/// `Present` returned for that frame.
pub const DOC_PRESENTED: &str = "doc_presented";
/// The scroll sweep began.
pub const SCROLL_BEGIN: &str = "scroll_begin";
/// The scroll sweep reached the end of the document.
pub const SCROLL_END: &str = "scroll_end";
/// The flick sweep (a fast but realistic step per frame, from the top) began.
pub const FLICK_BEGIN: &str = "flick_begin";
/// The flick sweep ended.
pub const FLICK_END: &str = "flick_end";
/// The scenario asked to open the folder.
pub const FOLDER_OPEN_BEGIN: &str = "folder_open_begin";
/// The folder is the sidebar's workspace.
pub const FOLDER_OPEN_RETURNED: &str = "folder_open_returned";
/// The first frame that draws the sidebar rows finished drawing.
pub const TREE_PAINTED: &str = "tree_painted";
/// `Present` returned for that frame.
pub const TREE_PRESENTED: &str = "tree_presented";
/// The folder became the workspace: the application indexes it from here on (the job starts on
/// the next housekeeping turn).
pub const INDEX_REQUESTED: &str = "index_requested";
/// The index job began on its worker.
pub const INDEX_STARTED: &str = "index_started";
/// The index job ended on its worker.
pub const INDEX_FINISHED: &str = "index_finished";
/// The first typed character was posted.
pub const TYPE_BEGIN: &str = "type_begin";
/// The last typed character's frame was presented.
pub const TYPE_END: &str = "type_end";

/// Frames before the scenario acts (startup, settling).
pub const PHASE_STARTUP: u8 = 0;
/// Frames while a document or folder opens.
pub const PHASE_OPEN: u8 = 1;
/// Frames of the scroll sweep.
pub const PHASE_SCROLL: u8 = 2;
/// Frames of the typing run.
pub const PHASE_TYPE: u8 = 3;
/// Frames of the flick sweep.
pub const PHASE_FLICK: u8 = 4;
/// The frame that takes the editor back to the top between the two sweeps (a cold frame that
/// belongs to neither).
pub const PHASE_JUMP: u8 = 5;
/// The names of the phases, indexed by phase number.
pub const PHASES: [&str; 6] = ["startup", "open", "scroll", "type", "flick", "jump"];

/// What a report says about the run besides the recorded data.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportMeta {
    /// The scenario name (`manual` for a plain run with `--perf-report`).
    pub scenario: String,
    /// The scenario ran to its end.
    pub completed: bool,
    /// Why it did not (set exactly when `completed` is false).
    pub error: Option<String>,
    /// The display refresh rate the run saw.
    pub refresh_hz: f64,
    /// Numbers describing the run (document size, rows, scroll step, ...).
    pub details: Vec<(&'static str, f64)>,
}

/// `value` rounded to microseconds (the reports stay small and readable).
fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// The values of one frame column, rounded.
fn column(frames: &[FrameSample], pick: impl Fn(&FrameSample) -> f64) -> Vec<f64> {
    frames.iter().map(|frame| round3(pick(frame))).collect()
}

/// `pairs` as a JSON object of rounded numbers.
fn numbers(pairs: &[(&'static str, f64)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), json!(round3(*value))))
        .collect()
}

/// The JSON report of a run.
#[must_use]
pub fn build_report(snapshot: &Snapshot, meta: &ReportMeta) -> Value {
    let phases: Vec<u8> = snapshot.frames.iter().map(|frame| frame.phase).collect();
    let frames = json!({
        "phase": phases,
        "start_ms": column(&snapshot.frames, |frame| frame.start_ms),
        "ready_ms": column(&snapshot.frames, |frame| frame.ready_ms),
        "recorded_ms": column(&snapshot.frames, |frame| frame.recorded_ms),
        "drawn_ms": column(&snapshot.frames, |frame| frame.drawn_ms),
        "presented_ms": column(&snapshot.frames, |frame| frame.presented_ms),
        "layout_ms": column(&snapshot.frames, |frame| frame.layout_ms),
        "editor_layout_ms": column(&snapshot.frames, |frame| frame.editor_layout_ms),
        "editor_paint_ms": column(&snapshot.frames, |frame| frame.editor_paint_ms),
    });
    let latency: Vec<f64> = snapshot
        .input_latency_ms
        .iter()
        .map(|ms| round3(*ms))
        .collect();
    json!({
        "schema": SCHEMA,
        "scenario": meta.scenario,
        "completed": meta.completed,
        "error": meta.error,
        "refresh_hz": round3(meta.refresh_hz),
        "process_age_at_start_ms": round3(snapshot.age_at_start_ms),
        "qpc_hz": snapshot.frequency,
        "details": numbers(&meta.details),
        "marks": numbers(&snapshot.marks),
        "phases": PHASES,
        "frames": frames,
        "input_latency_ms": latency,
    })
}

/// Writes `report` to `path` (missing folders are created).
///
/// # Errors
/// The folder or the file cannot be written.
pub fn write_report(path: &Path, report: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, report.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            frequency: 10_000_000,
            age_at_start_ms: 7.912_345,
            marks: vec![(WINDOW_SHOWN, 91.234_567), (DOC_PRESENTED, 400.0)],
            frames: vec![
                FrameSample {
                    phase: PHASE_STARTUP,
                    start_ms: 100.0,
                    ready_ms: 100.5,
                    recorded_ms: 103.0,
                    drawn_ms: 104.123_456,
                    presented_ms: 105.0,
                    layout_ms: 1.234_567,
                    editor_layout_ms: 0.5,
                    editor_paint_ms: 0.75,
                },
                FrameSample {
                    phase: PHASE_SCROLL,
                    start_ms: 120.0,
                    ready_ms: 121.0,
                    recorded_ms: 122.0,
                    drawn_ms: 123.0,
                    presented_ms: 133.333_333,
                    layout_ms: 0.0,
                    editor_layout_ms: 0.0,
                    editor_paint_ms: 0.0,
                },
            ],
            input_latency_ms: vec![12.345_678, 20.0],
        }
    }

    fn meta() -> ReportMeta {
        ReportMeta {
            scenario: "open-60k".to_owned(),
            completed: true,
            error: None,
            refresh_hz: 59.940_06,
            details: vec![("doc_bytes", 3_100_000.0), ("scroll_step_dip", 600.0)],
        }
    }

    #[test]
    fn the_report_carries_marks_frames_and_samples_rounded_to_microseconds() {
        let report = build_report(&snapshot(), &meta());
        assert_eq!(report["schema"], 1);
        assert_eq!(report["scenario"], "open-60k");
        assert_eq!(report["completed"], true);
        assert!(report["error"].is_null());
        assert_eq!(report["refresh_hz"], 59.94);
        assert_eq!(report["process_age_at_start_ms"], 7.912);
        assert_eq!(report["qpc_hz"], 10_000_000);
        assert_eq!(report["details"]["doc_bytes"], 3_100_000.0);
        assert_eq!(report["details"]["scroll_step_dip"], 600.0);
        assert_eq!(report["marks"]["window_shown"], 91.235);
        assert_eq!(report["marks"]["doc_presented"], 400.0);
        assert_eq!(
            report["phases"],
            json!(["startup", "open", "scroll", "type", "flick", "jump"])
        );
        assert_eq!(report["frames"]["phase"], json!([0, 2]));
        assert_eq!(report["frames"]["start_ms"], json!([100.0, 120.0]));
        assert_eq!(report["frames"]["drawn_ms"], json!([104.123, 123.0]));
        assert_eq!(report["frames"]["presented_ms"], json!([105.0, 133.333]));
        assert_eq!(report["frames"]["ready_ms"], json!([100.5, 121.0]));
        assert_eq!(report["frames"]["recorded_ms"], json!([103.0, 122.0]));
        assert_eq!(report["frames"]["layout_ms"], json!([1.235, 0.0]));
        assert_eq!(report["frames"]["editor_layout_ms"], json!([0.5, 0.0]));
        assert_eq!(report["frames"]["editor_paint_ms"], json!([0.75, 0.0]));
        assert_eq!(report["input_latency_ms"], json!([12.346, 20.0]));
    }

    #[test]
    fn a_failed_run_says_why_and_an_empty_run_has_empty_columns() {
        let failed = ReportMeta {
            completed: false,
            error: Some("timed out while scrolling".to_owned()),
            details: Vec::new(),
            ..meta()
        };
        let report = build_report(&Snapshot::default(), &failed);
        assert_eq!(report["completed"], false);
        assert_eq!(report["error"], "timed out while scrolling");
        assert_eq!(report["details"], json!({}));
        assert_eq!(report["marks"], json!({}));
        assert_eq!(report["frames"]["phase"], json!([]));
        assert_eq!(report["frames"]["presented_ms"], json!([]));
        assert_eq!(report["input_latency_ms"], json!([]));
    }

    #[test]
    fn the_phase_numbers_index_their_names() {
        for (phase, name) in [
            (PHASE_STARTUP, "startup"),
            (PHASE_OPEN, "open"),
            (PHASE_SCROLL, "scroll"),
            (PHASE_TYPE, "type"),
            (PHASE_FLICK, "flick"),
            (PHASE_JUMP, "jump"),
        ] {
            assert_eq!(PHASES[usize::from(phase)], name);
        }
    }

    #[test]
    fn a_report_is_written_as_json_below_missing_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a").join("b").join("report.json");
        let report = build_report(&snapshot(), &meta());
        write_report(&path, &report).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), report);
        // Writing again replaces the file.
        write_report(&path, &json!({ "again": 1 })).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({ "again": 1 })
        );
    }

    #[test]
    fn a_report_below_a_file_cannot_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "x").unwrap();
        let error = write_report(&file.join("report.json"), &json!({})).unwrap_err();
        assert!(!error.to_string().is_empty());
        // A directory is not a file either, and the empty path has no folder to create.
        assert!(write_report(tmp.path(), &json!({})).is_err());
        assert!(write_report(Path::new(""), &json!({})).is_err());
    }
}
