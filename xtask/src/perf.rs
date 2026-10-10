//! `cargo xtask perf`: builds the release executable, generates the fixtures (a 60 000-line
//! Markdown file and a 10 000-file folder tree below `target/perf`), runs every performance
//! scenario of `SSWriter.exe` several times (fresh profile per run, off-screen window, see
//! `docs/performance.md`) and writes `target/perf/report.md`: median and p95 of every metric
//! against the targets of spec §14. With `--write-docs` the results table of
//! `docs/performance.md` is replaced too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::cli::PerfArgs;
use crate::error::Error;
use crate::fsx;
use crate::metadata::Metadata;
use crate::run;
use crate::workspace::Workspace;

/// Lines of the scroll fixture.
pub const FIXTURE_LINES: usize = 60_000;
/// Top-level folders x sub-folders x files of the folder fixture: 10 x 10 x 100 = 10 000 files.
pub const TREE_SHAPE: (usize, usize, usize) = (10, 10, 100);
/// Opens the section of `docs/performance.md` that `--write-docs` replaces.
pub const DOCS_BEGIN: &str = "<!-- perf-results:begin -->";
/// Closes that section.
pub const DOCS_END: &str = "<!-- perf-results:end -->";

/// A run that has not ended after this long is killed (it is our own child process).
const RUN_TIMEOUT: Duration = Duration::from_secs(420);
/// Marker file of a complete folder fixture.
const TREE_DONE: &str = ".fixture-complete";

// ----- fixtures ----------------------------------------------------------------------------

/// A Markdown document of exactly `lines` lines with headings, paragraphs, links, lists, tables,
/// code blocks and quotes (deterministic).
pub fn markdown_fixture(lines: usize) -> String {
    let mut out: Vec<String> = Vec::with_capacity(lines + 16);
    let mut n = 0usize;
    while out.len() + 20 <= lines {
        n += 1;
        out.push(format!("## Section {n}: notes on topic {}", n % 97));
        out.push(String::new());
        out.push(format!(
            "Paragraph {n} has *emphasis*, **strong text**, `inline code` and a [link]({n}.md) to \
             [another page](https://example.com/page/{n}) with enough words to wrap at the usual \
             column width of the editor."
        ));
        out.push(format!(
            "A second sentence of paragraph {n} follows on the next line."
        ));
        out.push(String::new());
        for item in 1..=3 {
            out.push(format!(
                "- item {item} of section {n} with a [[wiki-{n}]] link"
            ));
        }
        out.push(String::new());
        out.push("| Name | Value | Note |".to_owned());
        out.push("| ---- | ----- | ---- |".to_owned());
        out.push(format!("| alpha | {n} | first |"));
        out.push(format!("| beta | {} | second |", n * 2));
        out.push(String::new());
        if n.is_multiple_of(3) {
            out.push("```rust".to_owned());
            out.push(format!("fn section_{n}() -> usize {{ {n} }}"));
            out.push("```".to_owned());
            out.push(String::new());
        } else if n % 3 == 1 {
            out.push(format!(
                "> Quote {n}: a short remark that is rendered as a block quote."
            ));
            out.push(String::new());
        }
    }
    let mut k = 0usize;
    while out.len() < lines {
        k += 1;
        out.push(format!("Closing line {k}."));
    }
    out.truncate(lines);
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Writes `top` x `sub` x `per` small Markdown files below `root`; returns the file count.
pub fn write_tree(root: &Path, shape: (usize, usize, usize)) -> Result<usize, Error> {
    let (top, sub, per) = shape;
    let mut count = 0;
    for a in 0..top {
        for b in 0..sub {
            let dir = root
                .join(format!("area-{a:02}"))
                .join(format!("topic-{b:02}"));
            for c in 0..per {
                let text = format!(
                    "# Note {a}-{b}-{c}\n\nText of note {c} in topic {b} of area {a}, with a \
                     link to [[note-{a}-{b}-{next}]] and a [web link](https://example.com/{c}).\n\n- point one\n- point two\n",
                    next = (c + 1) % per
                );
                fsx::write_file(&dir.join(format!("note-{a}-{b}-{c}.md")), text)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

// ----- statistics --------------------------------------------------------------------------

/// The `p`-th percentile (0..=100, nearest rank) of `values`; `None` when empty.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p * sorted.len() as f64 / 100.0).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

/// The median (mean of the two middle values for an even count).
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    })
}

// ----- metrics -----------------------------------------------------------------------------

/// The limit a metric is judged by (its p95 over the runs must not exceed it).
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// The largest passing value.
    pub limit: f64,
    /// Where the target comes from.
    pub source: String,
}

/// One measured quantity: a value per run (or per sample).
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    /// Metric name.
    pub name: &'static str,
    /// Values, unit ms unless the name says otherwise.
    pub values: Vec<f64>,
    /// The target, when the spec sets one.
    pub target: Option<Target>,
}

/// How a metric fares against its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// p95 within the limit.
    Pass,
    /// p95 above the limit, or no value at all.
    Fail,
    /// No target: reported only.
    Info,
}

impl Metric {
    /// The verdict: p95 of the values against the limit.
    pub fn verdict(&self) -> Verdict {
        match (&self.target, percentile(&self.values, 95.0)) {
            (None, _) => Verdict::Info,
            (Some(target), Some(p95)) if p95 <= target.limit => Verdict::Pass,
            _ => Verdict::Fail,
        }
    }
}

fn number(value: &Value, path: &[&str]) -> Option<f64> {
    path.iter()
        .try_fold(value, |node, key| node.get(*key))?
        .as_f64()
}

fn mark(run: &Value, name: &str) -> Option<f64> {
    number(run, &["marks", name])
}

fn between(run: &Value, from: &str, to: &str) -> Option<f64> {
    Some(mark(run, to)? - mark(run, from)?)
}

fn column(run: &Value, name: &str) -> Vec<f64> {
    run["frames"][name]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// `(draw ms, present time ms)` of the frames of `phase`, in order.
pub fn phase_frames(run: &Value, phase: u64) -> Vec<(f64, f64)> {
    let phases: Vec<u64> = run["frames"]["phase"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let (start, drawn, presented) = (
        column(run, "start_ms"),
        column(run, "drawn_ms"),
        column(run, "presented_ms"),
    );
    phases
        .iter()
        .enumerate()
        .filter(|(_, p)| **p == phase)
        .filter_map(|(i, _)| Some((drawn.get(i)? - start.get(i)?, *presented.get(i)?)))
        .collect()
}

/// The metrics of the runs of one scenario (`runs`: the parsed reports).
pub fn metrics_for(scenario: &str, runs: &[Value]) -> Vec<Metric> {
    let per_run =
        |f: &dyn Fn(&Value) -> Option<f64>| -> Vec<f64> { runs.iter().filter_map(f).collect() };
    let limit = |limit: f64, source: &str| {
        Some(Target {
            limit,
            source: source.to_owned(),
        })
    };
    let refresh_ms = runs
        .first()
        .and_then(|run| number(run, &["refresh_hz"]))
        .filter(|hz| *hz > 0.0)
        .map_or(1000.0 / 60.0, |hz| 1000.0 / hz);
    let metric = |name: &'static str, values: Vec<f64>, target: Option<Target>| Metric {
        name,
        values,
        target,
    };
    match scenario {
        "startup" => vec![
            metric(
                "window_frame_ms",
                per_run(&|r| mark(r, "window_shown")),
                limit(150.0, "§14: window frame with Mica < 150 ms"),
            ),
            metric(
                "first_content_frame_ms",
                per_run(&|r| mark(r, "first_content_present")),
                limit(370.0, "§14: bounded by device creation (205-370 ms)"),
            ),
            metric(
                "main_entered_ms",
                per_run(&|r| mark(r, "main_entered")),
                None,
            ),
            metric(
                "startup_done_ms",
                per_run(&|r| mark(r, "startup_done")),
                None,
            ),
            metric(
                "d3d_device_ready_ms",
                per_run(&|r| mark(r, "d3d_device_ready")),
                None,
            ),
            metric(
                "window_created_ms",
                per_run(&|r| mark(r, "window_created")),
                None,
            ),
            metric(
                "backdrop_applied_ms",
                per_run(&|r| mark(r, "backdrop_applied")),
                None,
            ),
            metric(
                "first_present_ms",
                per_run(&|r| mark(r, "first_present")),
                None,
            ),
        ],
        "open-60k" => {
            let scroll = |r: &Value| phase_frames(r, 2);
            let intervals =
                |r: &Value| -> Vec<f64> { scroll(r).windows(2).map(|w| w[1].1 - w[0].1).collect() };
            vec![
                metric(
                    "open_ms",
                    per_run(&|r| between(r, "doc_open_begin", "doc_presented")),
                    limit(300.0, "§14: open < 300 ms"),
                ),
                metric(
                    "open_read_ms",
                    per_run(&|r| between(r, "doc_open_begin", "doc_read")),
                    None,
                ),
                metric(
                    "open_decode_parse_ms",
                    per_run(&|r| between(r, "doc_read", "doc_parsed")),
                    None,
                ),
                metric(
                    "open_layout_paint_ms",
                    per_run(&|r| between(r, "doc_open_returned", "doc_painted")),
                    None,
                ),
                metric(
                    "open_present_ms",
                    per_run(&|r| between(r, "doc_painted", "doc_presented")),
                    None,
                ),
                metric(
                    "scroll_draw_p95_ms",
                    per_run(&|r| {
                        percentile(&scroll(r).iter().map(|f| f.0).collect::<Vec<_>>(), 95.0)
                    }),
                    limit(
                        refresh_ms,
                        "§14: scrolling at refresh rate: draw fits one refresh interval",
                    ),
                ),
                metric(
                    "scroll_interval_median_ms",
                    per_run(&|r| median(&intervals(r))),
                    limit(
                        refresh_ms * 1.1,
                        "§14: median frame interval <= 1.1 x refresh interval",
                    ),
                ),
                metric(
                    "scroll_interval_p95_ms",
                    per_run(&|r| percentile(&intervals(r), 95.0)),
                    limit(
                        refresh_ms * 1.5,
                        "§14: p95 frame interval <= 1.5 x refresh interval",
                    ),
                ),
                metric(
                    "scroll_frames",
                    per_run(&|r| Some(scroll(r).len() as f64)),
                    None,
                ),
            ]
        }
        "folder-10k" => vec![
            metric(
                "tree_visible_ms",
                per_run(&|r| between(r, "folder_open_begin", "tree_presented")),
                None,
            ),
            metric(
                "index_ms",
                per_run(&|r| between(r, "index_started", "index_finished")),
                None,
            ),
            metric(
                "index_wait_ms",
                per_run(&|r| between(r, "index_requested", "index_started")),
                None,
            ),
        ],
        _ => {
            let samples: Vec<f64> = runs
                .iter()
                .flat_map(|r| {
                    r["input_latency_ms"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|v| v.as_f64())
                .collect();
            vec![metric(
                "type_latency_ms",
                samples,
                limit(
                    2.0 * refresh_ms,
                    "§14: input-to-photon <= 2 frames (proxy: message to Present)",
                ),
            )]
        }
    }
}

/// The Markdown report: one table per scenario plus the verdict counts. Returns the text and
/// the number of failed targets.
pub fn render_report(
    sections: &[(&str, Vec<Metric>)],
    runs: usize,
    refresh_hz: Option<f64>,
) -> (String, usize) {
    let mut out = String::new();
    let mut failed = 0;
    out.push_str(&format!(
        "Runs per scenario: {runs}. Display refresh: {}. A target passes when the p95 over the runs (or over all samples for `type_latency_ms`) is within its limit.\n\n",
        refresh_hz.map_or("unknown".to_owned(), |hz| format!("{hz:.2} Hz"))
    ));
    for (scenario, metrics) in sections {
        out.push_str(&format!("### {scenario}\n\n| Metric | Target | Median | p95 | n | Verdict |\n|---|---|---:|---:|---:|---|\n"));
        for metric in metrics {
            let verdict = metric.verdict();
            failed += usize::from(verdict == Verdict::Fail);
            let cell = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{v:.2}"));
            let target = metric.target.as_ref().map_or("-".to_owned(), |t| {
                format!("<= {:.2} ({})", t.limit, t.source)
            });
            let label = match verdict {
                Verdict::Pass => "pass",
                Verdict::Fail => "FAIL",
                Verdict::Info => "info",
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                metric.name,
                target,
                cell(median(&metric.values)),
                cell(percentile(&metric.values, 95.0)),
                metric.values.len(),
                label
            ));
        }
        out.push('\n');
    }
    (out, failed)
}

/// `doc` with the text between [`DOCS_BEGIN`] and [`DOCS_END`] replaced by `body`.
pub fn replace_section(doc: &str, body: &str) -> Result<String, Error> {
    let missing = || {
        Error::Invalid(format!(
            "docs/performance.md needs {DOCS_BEGIN} and {DOCS_END}"
        ))
    };
    let begin = doc.find(DOCS_BEGIN).ok_or_else(missing)? + DOCS_BEGIN.len();
    let end = doc[begin..].find(DOCS_END).ok_or_else(missing)? + begin;
    Ok(format!(
        "{}\n\n{}\n{}",
        &doc[..begin],
        body.trim_end(),
        &doc[end..]
    ))
}

// ----- running -----------------------------------------------------------------------------

fn run_once(
    exe: &Path,
    work: &Path,
    scenario: &str,
    target: Option<&Path>,
    index: usize,
) -> Result<Value, String> {
    let profile = work.join("profiles").join(format!("{scenario}-{index}"));
    let _ = std::fs::remove_dir_all(&profile);
    let report = work.join("runs").join(format!("{scenario}-{index}.json"));
    let _ = std::fs::remove_file(&report);
    let mut command = Command::new(exe);
    command
        .args(["--test-profile"])
        .arg(&profile)
        .args([
            "--perf-offscreen",
            "--perf-scenario",
            scenario,
            "--perf-report",
        ])
        .arg(&report)
        .stdin(Stdio::null());
    if let Some(target) = target {
        command.arg("--perf-target").arg(target);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > RUN_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{scenario} run {index} timed out"));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let text = std::fs::read_to_string(&report)
        .map_err(|e| format!("{scenario} run {index} (exit {status}) wrote no report: {e}"))?;
    let json: Value =
        serde_json::from_str(&text).map_err(|e| format!("{scenario} run {index}: {e}"))?;
    if json["completed"] != Value::Bool(true) {
        return Err(format!(
            "{scenario} run {index} did not complete: {}",
            json["error"]
        ));
    }
    Ok(json)
}

/// Runs `cargo xtask perf`.
pub fn run(workspace: &Workspace, args: &PerfArgs) -> Result<(), Error> {
    let mut build = run::cargo();
    build
        .args(["build", "--release", "--locked", "-p", "ssw-app"])
        .current_dir(&workspace.root);
    run::run(&mut build)?;
    let metadata = Metadata::load(workspace)?;
    let exe = metadata
        .target_directory
        .join("release")
        .join("SSWriter.exe");
    let work: PathBuf = metadata.target_directory.join("perf");
    let doc = work.join("fixtures").join("sixty-thousand-lines.md");
    let tree = work.join("fixtures").join("tree");
    if !doc.is_file() {
        fsx::write_file(&doc, markdown_fixture(FIXTURE_LINES))?;
    }
    if !tree.join(TREE_DONE).is_file() {
        fsx::remove_dir_if_exists(&tree)?;
        write_tree(&tree, TREE_SHAPE)?;
        fsx::write_file(&tree.join(TREE_DONE), "")?;
    }
    let plan: [(&'static str, Option<&Path>); 4] = [
        ("startup", None),
        ("open-60k", Some(&doc)),
        ("folder-10k", Some(&tree)),
        ("type-latency", None),
    ];
    let mut sections = Vec::new();
    let mut problems = Vec::new();
    let mut refresh = None;
    for (scenario, target) in plan {
        let mut runs = Vec::new();
        for index in 0..args.runs {
            println!("perf: {scenario} run {}/{}", index + 1, args.runs);
            match run_once(&exe, &work, scenario, target, index) {
                Ok(json) => runs.push(json),
                Err(problem) => problems.push(problem),
            }
        }
        refresh = refresh.or_else(|| runs.first().and_then(|r| number(r, &["refresh_hz"])));
        sections.push((scenario, metrics_for(scenario, &runs)));
    }
    let (mut text, failed) = render_report(&sections, args.runs, refresh);
    if !problems.is_empty() {
        text.push_str("### Failed runs\n\n");
        for problem in &problems {
            text.push_str(&format!("- {problem}\n"));
        }
    }
    let report = format!("# Performance report\n\n{text}");
    fsx::write_file(&work.join("report.md"), &report)?;
    if args.write_docs {
        let path = workspace.path("docs/performance.md");
        fsx::write_file(&path, replace_section(&fsx::read_text(&path)?, &text)?)?;
    }
    println!(
        "perf: {} (targets failed: {failed}, runs failed: {})",
        work.join("report.md").display(),
        problems.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_markdown_fixture_has_exactly_the_requested_lines_and_every_block_kind() {
        let text = markdown_fixture(FIXTURE_LINES);
        assert_eq!(text.lines().count(), FIXTURE_LINES);
        assert!(text.ends_with('\n'));
        for needle in [
            "## Section 1:",
            "| Name | Value | Note |",
            "- item 2 of section",
            "```rust",
            "> Quote",
            "[[wiki-",
            "](https://example.com/",
        ] {
            assert!(text.contains(needle), "{needle}");
        }
        let fences = text.lines().filter(|l| l.starts_with("```")).count();
        assert_eq!(fences % 2, 0, "every code block is closed");
        assert_eq!(text, markdown_fixture(FIXTURE_LINES), "deterministic");
        assert_eq!(markdown_fixture(40).lines().count(), 40);
    }

    #[test]
    fn the_folder_fixture_writes_one_file_per_leaf_with_a_link_to_a_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(write_tree(tmp.path(), (2, 3, 4)).unwrap(), 24);
        let note = std::fs::read_to_string(
            tmp.path()
                .join("area-01")
                .join("topic-02")
                .join("note-1-2-3.md"),
        )
        .unwrap();
        assert!(note.starts_with("# Note 1-2-3\n"));
        assert!(note.contains("[[note-1-2-0]]"), "{note}");
        let count = |dir: &Path| std::fs::read_dir(dir).unwrap().count();
        assert_eq!(count(&tmp.path().join("area-00")), 3);
        assert_eq!(TREE_SHAPE.0 * TREE_SHAPE.1 * TREE_SHAPE.2, 10_000);
    }

    #[test]
    fn percentiles_and_medians_follow_the_nearest_rank_rule() {
        let v = [5.0, 1.0, 4.0, 2.0, 3.0];
        assert_eq!(median(&v), Some(3.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
        assert_eq!(percentile(&v, 95.0), Some(5.0));
        assert_eq!(percentile(&v, 50.0), Some(3.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        let hundred: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&hundred, 95.0), Some(95.0));
        assert_eq!(median(&[]), None);
        assert_eq!(percentile(&[], 95.0), None);
    }

    fn metric(values: Vec<f64>, limit: Option<f64>) -> Metric {
        Metric {
            name: "m_ms",
            values,
            target: limit.map(|limit| Target {
                limit,
                source: "spec".to_owned(),
            }),
        }
    }

    #[test]
    fn a_target_is_judged_by_the_p95_and_a_missing_value_fails() {
        assert_eq!(
            metric(vec![1.0, 2.0, 3.0], Some(3.0)).verdict(),
            Verdict::Pass
        );
        assert_eq!(
            metric(vec![1.0, 2.0, 3.1], Some(3.0)).verdict(),
            Verdict::Fail
        );
        assert_eq!(metric(vec![], Some(3.0)).verdict(), Verdict::Fail);
        assert_eq!(metric(vec![99.0], None).verdict(), Verdict::Info);
    }

    fn run_report() -> Value {
        json!({
            "completed": true, "refresh_hz": 100.0,
            "marks": { "window_shown": 90.0, "first_content_present": 300.0, "doc_open_begin": 1000.0, "doc_read": 1010.0, "doc_parsed": 1050.0, "doc_open_returned": 1060.0, "doc_painted": 1100.0, "doc_presented": 1120.0 },
            "frames": {
                "phase": [0, 2, 2, 2],
                "start_ms": [10.0, 2000.0, 2010.0, 2020.0],
                "drawn_ms": [12.0, 2004.0, 2013.0, 2026.0],
                "presented_ms": [13.0, 2005.0, 2015.0, 2040.0]
            },
            "input_latency_ms": [10.0, 30.0]
        })
    }

    #[test]
    fn metrics_come_from_marks_frames_and_samples() {
        let runs = [run_report(), run_report()];
        let startup = metrics_for("startup", &runs);
        assert_eq!(startup[0].values, [90.0, 90.0]);
        assert_eq!(startup[0].target.as_ref().unwrap().limit, 150.0);
        assert_eq!(startup[1].values, [300.0, 300.0]);
        let open = metrics_for("open-60k", &runs);
        let get = |name: &str| open.iter().find(|m| m.name == name).unwrap().clone();
        assert_eq!(get("open_ms").values, [120.0, 120.0]);
        assert_eq!(get("open_layout_paint_ms").values, [40.0, 40.0]);
        // Draws 4, 3, 6 ms; intervals 10 and 25 ms; refresh 10 ms.
        assert_eq!(get("scroll_draw_p95_ms").values, [6.0, 6.0]);
        assert_eq!(get("scroll_interval_p95_ms").values, [25.0, 25.0]);
        assert_eq!(get("scroll_draw_p95_ms").target.unwrap().limit, 10.0);
        assert_eq!(get("scroll_interval_p95_ms").verdict(), Verdict::Fail);
        assert_eq!(get("scroll_draw_p95_ms").verdict(), Verdict::Pass);
        let typing = metrics_for("type-latency", &runs);
        assert_eq!(typing[0].values, [10.0, 30.0, 10.0, 30.0]);
        assert_eq!(typing[0].target.as_ref().unwrap().limit, 20.0);
        assert_eq!(typing[0].verdict(), Verdict::Fail);
        // A run without the marks contributes nothing instead of a bogus value.
        let sparse = metrics_for("folder-10k", &[json!({ "marks": {} })]);
        assert!(sparse.iter().all(|m| m.values.is_empty()));
    }

    #[test]
    fn the_report_lists_verdicts_and_counts_failures() {
        let sections = [(
            "startup",
            vec![
                metric(vec![200.0, 210.0], Some(150.0)),
                metric(vec![5.0], None),
            ],
        )];
        let (text, failed) = render_report(&sections, 5, Some(60.0));
        assert_eq!(failed, 1);
        assert!(text.contains("### startup"));
        assert!(
            text.contains("| m_ms | <= 150.00 (spec) | 205.00 | 210.00 | 2 | FAIL |"),
            "{text}"
        );
        assert!(
            text.contains("| m_ms | - | 5.00 | 5.00 | 1 | info |"),
            "{text}"
        );
        assert!(text.contains("Runs per scenario: 5. Display refresh: 60.00 Hz"));
        assert!(render_report(&[], 1, None).0.contains("refresh: unknown"));
    }

    #[test]
    fn the_docs_section_between_the_markers_is_replaced() {
        let doc = format!("intro\n{DOCS_BEGIN}\nold table\n{DOCS_END}\noutro\n");
        let out = replace_section(&doc, "new table\n").unwrap();
        assert_eq!(
            out,
            format!("intro\n{DOCS_BEGIN}\n\nnew table\n{DOCS_END}\noutro\n")
        );
        assert_eq!(
            replace_section(&out, "new table\n").unwrap(),
            out,
            "idempotent"
        );
        assert!(replace_section("no markers", "x").is_err());
        assert!(replace_section(&format!("{DOCS_BEGIN} only"), "x").is_err());
    }
}
