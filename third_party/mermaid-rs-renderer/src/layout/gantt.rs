use super::*;
use crate::gantt_time::{
    DateFormat, add_duration, auto_ticks, format_instant, parse_duration, parse_tick_interval,
};

/// Font size of the axis labels (the renderer draws them at the same size).
fn gantt_tick_font(theme: &Theme) -> f32 {
    theme.font_size * 0.8
}

fn gantt_palette(theme: &Theme) -> Vec<String> {
    vec![
        theme.primary_border_color.clone(),
        "#0ea5e9".to_string(), // sky-500
        "#10b981".to_string(), // emerald-500
        "#6366f1".to_string(), // indigo-500
        "#f97316".to_string(), // orange-500
    ]
}

fn hsl_color(h: f32, s: f32, l: f32) -> String {
    format!("hsl({:.10}, {:.10}%, {:.10}%)", h, s, l)
}

fn shift_color(color: &str, target_s: f32, target_l: f32, strength: f32) -> String {
    let Some((_h, s, l)) = parse_color_to_hsl(color) else {
        return color.to_string();
    };
    let delta_s = (target_s - s) * strength;
    let delta_l = (target_l - l) * strength;
    adjust_color(color, 0.0, delta_s, delta_l)
}

fn gantt_section_palette(theme: &Theme, sections: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if sections.is_empty() {
        return map;
    }
    let base = theme.primary_border_color.as_str();
    let step = 360.0 / sections.len().max(1) as f32;
    for (idx, name) in sections.iter().enumerate() {
        let hue_shift = step * idx as f32;
        let mut color = adjust_color(base, hue_shift, 0.0, 0.0);
        color = shift_color(&color, 60.0, 55.0, 0.4);
        map.insert(name.clone(), color);
    }
    map
}

fn gantt_task_color(status: Option<crate::ir::GanttStatus>, base: &str, fallback: &str) -> String {
    let base = if parse_color_to_hsl(base).is_some() {
        base.to_string()
    } else {
        fallback.to_string()
    };
    match status {
        Some(crate::ir::GanttStatus::Done) => shift_color(&base, 30.0, 80.0, 0.7),
        Some(crate::ir::GanttStatus::Active) => shift_color(&base, 70.0, 52.0, 0.6),
        Some(crate::ir::GanttStatus::Crit) => {
            if let Some((_, s, l)) = parse_color_to_hsl(&base) {
                hsl_color(0.0, s.max(65.0), l.clamp(45.0, 60.0))
            } else {
                "#ef4444".to_string()
            }
        }
        Some(crate::ir::GanttStatus::Milestone) => {
            if let Some((_, s, l)) = parse_color_to_hsl(&base) {
                hsl_color(45.0, s.max(65.0), l.clamp(50.0, 65.0))
            } else {
                "#f59e0b".to_string()
            }
        }
        None => base,
    }
}

/// A task as laid out: label, start day, end day, status, section.
type GanttRow<'a> = (
    &'a str,
    f64,
    f64,
    Option<crate::ir::GanttStatus>,
    Option<String>,
);

/// Default length of a task that names neither a duration nor an end date.
const DEFAULT_TASK_DAYS: f64 = 3.0;

/// When a task runs, in days since 1970-01-01 (`end >= start`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct GanttSpan {
    pub(super) start: f64,
    pub(super) end: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct GanttSchedule {
    /// One span per task, in definition order.
    pub(super) spans: Vec<GanttSpan>,
    /// Whether any task has a calendar start date; otherwise the spans are
    /// plain day offsets from zero and the axis prints numbers, not dates.
    pub(super) has_dates: bool,
}

/// Resolve every task to a `[start, end]` span the way Mermaid does.
///
/// * start: the task's own date, else the latest end of the tasks it is
///   `after`, else the end of the previous task in the source (any section),
///   else the earliest explicit date of the chart;
/// * end: its end date, else the start of the task it runs `until`, else
///   start + duration (calendar months/years), else start (milestone) or
///   start + three days.
///
/// References may point forward; a reference cycle or unknown id degrades to
/// "after the previous task" instead of failing.
pub(super) fn schedule_gantt(
    tasks: &[crate::ir::GanttTask],
    date_format: &DateFormat,
) -> GanttSchedule {
    let starts: Vec<Option<f64>> = tasks
        .iter()
        .map(|t| t.start.as_deref().and_then(|s| date_format.parse(s)))
        .collect();
    let ends: Vec<Option<f64>> = tasks
        .iter()
        .map(|t| t.end.as_deref().and_then(|s| date_format.parse(s)))
        .collect();
    let has_dates = starts.iter().any(Option::is_some);
    let base = starts
        .iter()
        .flatten()
        .copied()
        .reduce(f64::min)
        .unwrap_or(0.0);

    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, task) in tasks.iter().enumerate() {
        index.insert(task.id.as_str(), i);
    }
    let after_ids = |i: usize| -> Vec<usize> {
        tasks[i]
            .after
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|id| index.get(id).copied())
            .filter(|&j| j != i)
            .collect()
    };
    let until_id = |i: usize| -> Option<usize> {
        tasks[i]
            .until
            .as_deref()
            .and_then(|id| index.get(id.trim()).copied())
            .filter(|&j| j != i)
    };

    // Tasks whose spans a task needs before it can be placed.
    let deps: Vec<Vec<usize>> = (0..tasks.len())
        .map(|i| {
            let mut needed = Vec::new();
            if starts[i].is_none() {
                let after = after_ids(i);
                if !after.is_empty() {
                    needed.extend(after);
                } else if i > 0 {
                    needed.push(i - 1);
                }
            }
            if ends[i].is_none()
                && let Some(j) = until_id(i)
            {
                needed.push(j);
            }
            needed
        })
        .collect();

    let place = |i: usize, done: &[Option<GanttSpan>]| -> GanttSpan {
        let task = &tasks[i];
        let start = starts[i].unwrap_or_else(|| {
            after_ids(i)
                .into_iter()
                .filter_map(|j| done[j])
                .map(|span| span.end)
                .reduce(f64::max)
                .unwrap_or_else(|| {
                    if i > 0 {
                        done[i - 1].map_or(base, |span| span.end)
                    } else {
                        base
                    }
                })
        });
        let end = ends[i]
            .or_else(|| until_id(i).and_then(|j| done[j]).map(|span| span.start))
            .unwrap_or_else(|| match task.duration.as_deref().and_then(parse_duration) {
                Some(duration) => add_duration(start, duration),
                None if task.status == Some(crate::ir::GanttStatus::Milestone) => start,
                None => start + DEFAULT_TASK_DAYS,
            });
        GanttSpan {
            start,
            end: end.max(start),
        }
    };

    let mut done: Vec<Option<GanttSpan>> = vec![None; tasks.len()];
    let mut remaining = tasks.len();
    while remaining > 0 {
        let mut progressed = false;
        for i in 0..tasks.len() {
            if done[i].is_none() && deps[i].iter().all(|&j| done[j].is_some()) {
                done[i] = Some(place(i, &done));
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            // Dependency cycle: place the first open task, ignoring its open references.
            let Some(i) = done.iter().position(Option::is_none) else {
                break;
            };
            done[i] = Some(place(i, &done));
            remaining -= 1;
        }
    }

    GanttSchedule {
        spans: done.into_iter().flatten().collect(),
        has_dates,
    }
}

/// Axis ticks for a dated chart: the user's `tickInterval` when it yields
/// ticks, else the finest calendar spacing at which the labels do not collide.
fn gantt_date_ticks(
    time_start: f64,
    time_end: f64,
    time_scale: f64,
    axis_format: &str,
    tick_interval: Option<&str>,
    theme: &Theme,
    config: &LayoutConfig,
) -> Vec<(f64, String)> {
    let tick_font = gantt_tick_font(theme);
    let label_gap = tick_font;
    let fits = |ticks: &[f64]| -> bool {
        let widest = ticks
            .iter()
            .map(|&t| {
                measure_label_with_font_size(
                    &format_instant(axis_format, t),
                    tick_font,
                    config,
                    false,
                    theme.font_family.as_str(),
                )
                .width
            })
            .fold(0.0_f32, f32::max);
        let closest = ticks
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .fold(f64::INFINITY, f64::min);
        closest * time_scale >= f64::from(widest + label_gap)
    };
    let requested = tick_interval
        .and_then(parse_tick_interval)
        .and_then(|interval| interval.ticks(time_start, time_end))
        .filter(|ticks| !ticks.is_empty());
    requested
        .unwrap_or_else(|| auto_ticks(time_start, time_end, fits))
        .into_iter()
        .map(|t| (t, format_instant(axis_format, t)))
        .collect()
}

pub(super) fn compute_gantt_layout(graph: &Graph, theme: &Theme, config: &LayoutConfig) -> Layout {
    let padding = theme.font_size * 1.25;
    let row_height = (theme.font_size * 1.5).max(theme.font_size + 8.0);
    let label_gap = theme.font_size * 1.05;

    let title = graph
        .gantt_title
        .as_ref()
        .map(|t| measure_label(t, theme, config));
    let title_height = title.as_ref().map(|t| t.height + padding).unwrap_or(0.0);

    let mut task_label_width = 0.0_f32;
    let mut section_label_width = 0.0_f32;
    for task in &graph.gantt_tasks {
        let label = measure_label(&task.label, theme, config);
        task_label_width = task_label_width.max(label.width);
        if let Some(section) = task.section.as_ref() {
            let section_label = measure_label(section, theme, config);
            section_label_width = section_label_width.max(section_label.width);
        }
    }
    task_label_width = task_label_width.max(theme.font_size * 6.5);

    let label_x = padding;
    let section_task_gap = if section_label_width > 0.0 {
        theme.font_size * 0.8
    } else {
        0.0
    };
    let label_width = section_label_width + section_task_gap + task_label_width;
    let section_label_x = label_x;
    let task_label_x = label_x + section_label_width + section_task_gap;
    let chart_x = padding + label_width + label_gap;
    let chart_y = title_height + padding;
    let chart_width = theme.font_size * 26.0;

    let date_format = graph
        .gantt_date_format
        .as_deref()
        .map(DateFormat::new)
        .unwrap_or_default();
    let GanttSchedule { spans, has_dates } = schedule_gantt(&graph.gantt_tasks, &date_format);

    let mut time_start = spans.iter().map(|s| s.start).fold(f64::INFINITY, f64::min);
    let mut time_end = spans
        .iter()
        .map(|s| s.end)
        .fold(f64::NEG_INFINITY, f64::max);
    if !time_start.is_finite() || !time_end.is_finite() {
        time_start = 0.0;
        time_end = 1.0;
    }
    if time_end - time_start < 1.0e-9 {
        // Only milestones (or nothing): give the axis a day so it has a scale.
        time_end = time_start + 1.0;
    }
    let time_span = time_end - time_start;
    let time_scale = f64::from(chart_width) / time_span;

    let ticks: Vec<GanttTick> = if has_dates {
        gantt_date_ticks(
            time_start,
            time_end,
            time_scale,
            graph.gantt_axis_format.as_deref().unwrap_or("%Y-%m-%d"),
            graph.gantt_tick_interval.as_deref(),
            theme,
            config,
        )
        .into_iter()
        .map(|(t, label)| GanttTick {
            x: chart_x + ((t - time_start) * time_scale) as f32,
            label,
        })
        .collect()
    } else {
        // No calendar dates: the axis counts days from the first task.
        let tick_count = 4;
        (0..=tick_count)
            .map(|i| {
                let offset = time_span * f64::from(i) / f64::from(tick_count);
                GanttTick {
                    x: chart_x + (offset * time_scale) as f32,
                    label: format!("{offset:.0}"),
                }
            })
            .collect()
    };

    let computed: Vec<GanttRow> = graph
        .gantt_tasks
        .iter()
        .zip(&spans)
        .map(|(task, span)| {
            (
                task.label.as_str(),
                span.start,
                span.end,
                task.status,
                task.section.clone(),
            )
        })
        .collect();

    let compact = graph
        .gantt_display_mode
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("compact"))
        .unwrap_or(false);

    let palette = gantt_palette(theme);
    let section_palette = gantt_section_palette(theme, &graph.gantt_sections);
    let mut current_section: Option<String> = None;
    let mut current_section_idx: Option<usize> = None;
    let mut sections: Vec<GanttSectionLayout> = Vec::new();
    let mut tasks: Vec<GanttTaskLayout> = Vec::new();
    let mut y = chart_y;

    // Compact mode: pack non-overlapping tasks into the same row.
    // Each lane is y_position, end_time
    let mut lanes: Vec<(f32, f64)> = Vec::new();

    for (idx, (label, start, end, status, section)) in computed.iter().enumerate() {
        if section != &current_section {
            if let Some(sec) = section.as_ref() {
                if let Some(prev_idx) = current_section_idx {
                    let height = (y - sections[prev_idx].y).max(row_height);
                    sections[prev_idx].height = height;
                }
                lanes.clear();
                let base_color = section_palette
                    .get(sec)
                    .cloned()
                    .unwrap_or_else(|| palette[idx % palette.len()].clone());
                let band_color = shift_color(&base_color, 20.0, 92.0, 0.7);
                sections.push(GanttSectionLayout {
                    label: measure_label(sec, theme, config),
                    y,
                    height: 0.0,
                    color: base_color,
                    band_color,
                });
                current_section_idx = Some(sections.len() - 1);
            } else if let Some(prev_idx) = current_section_idx {
                let height = (y - sections[prev_idx].y).max(row_height);
                sections[prev_idx].height = height;
                current_section_idx = None;
                lanes.clear();
            }
            current_section = section.clone();
        }

        let bar_x = chart_x + ((start - time_start) * time_scale) as f32;
        let mut bar_width = ((end - start) * time_scale) as f32;
        let min_width = row_height * 0.5;
        if bar_width < min_width {
            bar_width = min_width;
        }
        let base_color = if let Some(sec) = section.as_ref() {
            section_palette
                .get(sec)
                .cloned()
                .unwrap_or_else(|| palette[idx % palette.len()].clone())
        } else {
            palette[idx % palette.len()].clone()
        };
        let color = gantt_task_color(*status, &base_color, &palette[0]);

        let task_y = if compact {
            if let Some(lane) = lanes.iter_mut().find(|(_, lane_end)| *start >= *lane_end) {
                let ly = lane.0;
                lane.1 = *end;
                ly
            } else {
                let ly = y;
                lanes.push((ly, *end));
                y += row_height;
                ly
            }
        } else {
            let ly = y;
            y += row_height;
            ly
        };

        tasks.push(GanttTaskLayout {
            label: measure_label(label, theme, config),
            x: bar_x,
            y: task_y,
            width: bar_width,
            height: row_height,
            color,
            start: *start as f32,
            duration: (end - start) as f32,
            status: *status,
        });
    }
    if let Some(prev_idx) = current_section_idx {
        let height = (y - sections[prev_idx].y).max(row_height);
        sections[prev_idx].height = height;
    }

    let tick_font = gantt_tick_font(theme);
    let max_tick_half_width = ticks
        .iter()
        .map(|tick| {
            measure_label_with_font_size(
                tick.label.as_str(),
                tick_font,
                config,
                false,
                theme.font_family.as_str(),
            )
            .width
                / 2.0
        })
        .fold(0.0_f32, f32::max);
    let axis_pad = row_height * 0.9 + theme.font_size;
    let height = y + padding + axis_pad;
    let label_overflow = if compact {
        padding + task_label_width
    } else {
        0.0
    };
    let right_padding = (max_tick_half_width + padding * 0.4)
        .max(label_overflow)
        .max(padding);
    let width = chart_x + chart_width + right_padding;

    Layout {
        kind: graph.kind,
        nodes: BTreeMap::new(),
        edges: Vec::new(),
        subgraphs: Vec::new(),
        diagram: DiagramData::Gantt(GanttLayout {
            title,
            sections,
            tasks,
            time_start: time_start as f32,
            time_end: time_end as f32,
            chart_x,
            chart_y,
            chart_width,
            chart_height: y - chart_y,
            row_height,
            label_x,
            label_width,
            section_label_x,
            section_label_width,
            task_label_x,
            task_label_width,
            title_y: chart_y - row_height * 0.6,
            ticks,
            compact,
        }),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use crate::gantt_time::DateFormat;
    use crate::ir::{DiagramKind, GanttStatus, GanttTask, Graph};
    use crate::layout::LayoutConfig;
    use crate::layout::types::{DiagramData, GanttLayout};
    use crate::parser::parse_mermaid;
    use crate::theme::Theme;

    use super::{compute_gantt_layout, schedule_gantt};

    /// The example from the Mermaid documentation (`docs/gantt`), the input the
    /// spike rendered with a 44-year axis and zero-width bars.
    const DOCS_EXAMPLE: &str = "gantt
    title A Gantt Diagram
    dateFormat YYYY-MM-DD
    axisFormat %m/%d
    section Section
        A task          :a1, 2014-01-01, 30d
        Another task    :after a1, 20d
        Done task       :done, d1, 2014-01-03, 5d
        Crit task       :crit, active, c1, 2014-01-10, 10d
    section Another
        Task in Another :2014-01-12, 12d
        Milestone       :milestone, m1, 2014-01-20, 0d
        another task    :24d
";

    fn day(text: &str) -> f64 {
        DateFormat::default().parse(text).unwrap()
    }

    fn schedule_of(source: &str) -> super::GanttSchedule {
        let graph = parse_mermaid(source).unwrap().graph;
        let format = graph
            .gantt_date_format
            .as_deref()
            .map(DateFormat::new)
            .unwrap_or_default();
        schedule_gantt(&graph.gantt_tasks, &format)
    }

    fn spans_of(source: &str) -> Vec<(f64, f64)> {
        schedule_of(source)
            .spans
            .iter()
            .map(|span| (span.start, span.end))
            .collect()
    }

    fn layout_of(source: &str) -> GanttLayout {
        let graph = parse_mermaid(source).unwrap().graph;
        match compute_gantt_layout(&graph, &Theme::modern(), &LayoutConfig::default()).diagram {
            DiagramData::Gantt(gantt) => gantt,
            _ => panic!("expected Gantt layout"),
        }
    }

    fn make_graph(display_mode: Option<&str>, tasks: Vec<GanttTask>) -> Graph {
        let mut graph = Graph::new();
        graph.kind = DiagramKind::Gantt;
        graph.gantt_display_mode = display_mode.map(|s| s.to_string());
        let mut sections = Vec::new();
        for t in &tasks {
            if let Some(sec) = &t.section
                && !sections.contains(sec)
            {
                sections.push(sec.clone());
            }
        }
        graph.gantt_sections = sections;
        graph.gantt_tasks = tasks;
        graph
    }

    fn task(id: &str, section: &str, start: &str, dur: &str) -> GanttTask {
        GanttTask {
            id: id.to_string(),
            label: id.to_string(),
            start: Some(start.to_string()),
            duration: Some(dur.to_string()),
            end: None,
            after: None,
            until: None,
            section: Some(section.to_string()),
            status: None,
        }
    }

    fn milestone(id: &str, section: &str, start: &str) -> GanttTask {
        GanttTask {
            id: id.to_string(),
            label: id.to_string(),
            start: Some(start.to_string()),
            duration: Some("0d".to_string()),
            end: None,
            after: None,
            until: None,
            section: Some(section.to_string()),
            status: Some(GanttStatus::Milestone),
        }
    }

    fn extract_task_ys(graph: &Graph) -> Vec<f32> {
        let theme = Theme::modern();
        let config = LayoutConfig::default();
        let layout = compute_gantt_layout(graph, &theme, &config);
        match &layout.diagram {
            DiagramData::Gantt(g) => g.tasks.iter().map(|t| t.y).collect(),
            _ => panic!("expected Gantt layout"),
        }
    }

    #[test]
    fn compact_non_overlapping_tasks_share_row() {
        let graph = make_graph(
            Some("compact"),
            vec![
                milestone("m1", "MacOS", "2025-09-01"),
                milestone("m2", "MacOS", "2026-09-01"),
                milestone("m3", "MacOS", "2027-09-01"),
            ],
        );
        let ys = extract_task_ys(&graph);
        assert_eq!(ys[0], ys[1], "m1 and m2 should share a row");
        assert_eq!(ys[1], ys[2], "m2 and m3 should share a row");
    }

    #[test]
    fn compact_overlapping_tasks_get_separate_rows() {
        let graph = make_graph(
            Some("compact"),
            vec![
                task("hw", "support", "2025-09-01", "1y"),
                task("sw", "support", "2025-09-01", "2y"),
            ],
        );
        let ys = extract_task_ys(&graph);
        assert_ne!(ys[0], ys[1], "overlapping tasks must be on different rows");
    }

    #[test]
    fn non_compact_always_separate_rows() {
        let graph = make_graph(
            None,
            vec![
                milestone("m1", "MacOS", "2025-09-01"),
                milestone("m2", "MacOS", "2026-09-01"),
                milestone("m3", "MacOS", "2027-09-01"),
            ],
        );
        let ys = extract_task_ys(&graph);
        assert_ne!(ys[0], ys[1]);
        assert_ne!(ys[1], ys[2]);
    }

    #[test]
    fn docs_example_schedules_every_task_on_its_own_dates() {
        let expected = [
            ("2014-01-01", "2014-01-31"), // A task, 30d
            ("2014-01-31", "2014-02-20"), // after a1, 20d
            ("2014-01-03", "2014-01-08"), // Done task, 5d
            ("2014-01-10", "2014-01-20"), // Crit task, 10d
            ("2014-01-12", "2014-01-24"), // Task in Another, 12d
            ("2014-01-20", "2014-01-20"), // Milestone, 0d
            ("2014-01-20", "2014-02-13"), // 24d, starts when the milestone ends
        ];
        let spans = spans_of(DOCS_EXAMPLE);
        assert_eq!(spans.len(), expected.len());
        for (i, ((start, end), (want_start, want_end))) in spans.iter().zip(expected).enumerate() {
            assert_eq!(*start, day(want_start), "task {i} start");
            assert_eq!(*end, day(want_end), "task {i} end");
        }
    }

    #[test]
    fn docs_example_axis_spans_fifty_days_and_bars_have_their_width() {
        let layout = layout_of(DOCS_EXAMPLE);
        assert_eq!(layout.time_start, day("2014-01-01") as f32);
        assert_eq!(layout.time_end, day("2014-02-20") as f32);
        let chart = layout.chart_width;
        let near = |got: f32, want: f32| (got - want).abs() < 0.01;
        let offset = |i: usize| layout.tasks[i].x - layout.chart_x;
        let width = |i: usize| layout.tasks[i].width;
        // 50 days across the chart: A task 30d, "after a1" 20d, Done task 5d.
        assert!(near(offset(0), 0.0) && near(width(0), chart * 30.0 / 50.0));
        assert!(near(offset(1), chart * 30.0 / 50.0) && near(width(1), chart * 20.0 / 50.0));
        assert!(near(offset(2), chart * 2.0 / 50.0) && near(width(2), chart * 5.0 / 50.0));
        assert!(near(offset(5), chart * 19.0 / 50.0), "the milestone sits on 2014-01-20");
        assert!(near(offset(6), chart * 19.0 / 50.0) && near(width(6), chart * 24.0 / 50.0));
    }

    #[test]
    fn docs_example_axis_labels_use_axis_format_and_stay_on_the_chart() {
        let layout = layout_of(DOCS_EXAMPLE);
        assert!(layout.ticks.len() >= 2, "{:?}", layout.ticks);
        let right = layout.chart_x + layout.chart_width;
        let month_day = |label: &str| -> (u32, u32) {
            let bytes = label.as_bytes();
            assert!(
                bytes.len() == 5
                    && bytes[2] == b'/'
                    && bytes[..2].iter().all(u8::is_ascii_digit)
                    && bytes[3..].iter().all(u8::is_ascii_digit),
                "{label:?} is not %m/%d"
            );
            (label[..2].parse().unwrap(), label[3..].parse().unwrap())
        };
        let dates: Vec<(u32, u32)> = layout.ticks.iter().map(|t| month_day(&t.label)).collect();
        assert!(dates.windows(2).all(|pair| pair[0] < pair[1]), "{dates:?}");
        assert!(dates[0] >= (1, 1) && *dates.last().unwrap() <= (2, 20), "{dates:?}");
        for tick in &layout.ticks {
            assert!(tick.x >= layout.chart_x - 0.01 && tick.x <= right + 0.01, "{tick:?}");
        }
        assert!(layout.ticks.windows(2).all(|pair| pair[0].x < pair[1].x));
    }

    #[test]
    fn axis_format_and_tick_interval_directives_are_honoured() {
        let named = layout_of(&DOCS_EXAMPLE.replace("%m/%d", "%b %d"));
        assert!(named.ticks.len() >= 2);
        for tick in &named.ticks {
            assert!(
                tick.label.starts_with("Jan ") || tick.label.starts_with("Feb "),
                "{tick:?}"
            );
        }
        let iso = layout_of(&DOCS_EXAMPLE.replace("    axisFormat %m/%d\n", ""));
        for tick in &iso.ticks {
            assert!(tick.label.starts_with("2014-"), "default axisFormat is %Y-%m-%d: {tick:?}");
        }
        let weekly = layout_of(
            &DOCS_EXAMPLE
                .replace("%m/%d", "%a")
                .replace("axisFormat", "tickInterval 1week\n    axisFormat"),
        );
        assert_eq!(weekly.ticks.len(), 7, "Sundays from 2014-01-05 to 2014-02-16");
        assert!(weekly.ticks.iter().all(|t| t.label == "Sun"), "{:?}", weekly.ticks);
    }

    #[test]
    fn rendered_docs_example_has_no_decades_long_axis() {
        // Spike output: axis labels 2014-01-01 .. 2058-03-17 and zero-width bars.
        let svg = crate::render(DOCS_EXAMPLE).unwrap();
        assert!(!svg.contains("2014-01-01") && !svg.contains("2058"));
        assert!(svg.contains(">01/") || svg.contains(">02/"), "month/day tick labels");
    }

    #[test]
    fn task_without_start_follows_the_previous_task() {
        // Regression: the running cursor held an absolute day number and was added to
        // the chart origin a second time, so every task without a date started a
        // few decades late and stretched the axis.
        let source = "gantt\n  dateFormat YYYY-MM-DD\n  Alpha :a, 2024-03-01, 10d\n  Beta :5d\n";
        let spans = spans_of(source);
        assert_eq!(spans[0], (day("2024-03-01"), day("2024-03-11")));
        assert_eq!(spans[1], (day("2024-03-11"), day("2024-03-16")));
        assert_eq!(layout_of(source).time_end, day("2024-03-16") as f32);
    }

    #[test]
    fn end_dates_defaults_and_milestones() {
        let source = "gantt
  Dated :a, 2024-03-01, 2024-03-15
  Open :2024-04-01
  Release :milestone, 2024-05-01
";
        let spans = spans_of(source);
        assert_eq!(spans[0], (day("2024-03-01"), day("2024-03-15")));
        assert_eq!(spans[1], (day("2024-04-01"), day("2024-04-04")), "default length is 3 days");
        assert_eq!(spans[2], (day("2024-05-01"), day("2024-05-01")), "a milestone has no length");
    }

    #[test]
    fn date_format_directive_applies_wherever_it_is_declared() {
        let source = "gantt\n  Alpha :a, 01-03-2024, 10d\n  dateFormat DD-MM-YYYY\n";
        assert_eq!(spans_of(source)[0], (day("2024-03-01"), day("2024-03-11")));
    }

    #[test]
    fn calendar_months_and_clock_times_are_scheduled_exactly() {
        let months = spans_of("gantt\n  Alpha :a, 2024-01-31, 1M\n  Beta :b, 2024-01-31, 1y\n");
        assert_eq!(months[0].1, day("2024-02-29"), "2024 is a leap year");
        assert_eq!(months[1].1, day("2025-01-31"));

        let source = "gantt
  dateFormat YYYY-MM-DD HH:mm
  axisFormat %H:%M
  Alpha :a, 2024-03-01 09:00, 2h
  Beta :after a, 30m
";
        let spans = spans_of(source);
        let nine = day("2024-03-01") + 9.0 / 24.0;
        assert!((spans[0].0 - nine).abs() < 1e-9);
        assert!((spans[0].1 - (nine + 2.0 / 24.0)).abs() < 1e-9);
        assert_eq!(spans[1].0, spans[0].1);
        assert!((spans[1].1 - (nine + 2.5 / 24.0)).abs() < 1e-9);
        let layout = layout_of(source);
        assert!(layout.ticks.len() >= 2);
        let labels: Vec<&str> = layout.ticks.iter().map(|t| t.label.as_str()).collect();
        for label in &labels {
            assert!(label.len() == 5 && label.as_bytes()[2] == b':', "{label:?}");
        }
        assert!(labels.windows(2).all(|pair| pair[0] < pair[1]), "{labels:?}");
        assert!(labels[0] >= "09:00" && labels[labels.len() - 1] <= "11:30", "{labels:?}");
    }

    #[test]
    fn references_use_exact_ids_forward_references_and_the_latest_end() {
        let source = "gantt
  dateFormat YYYY-MM-DD
  Late join :j, after Task-1 B2, 2d
  Task one :Task-1, 2024-01-01, 3d
  Second :B2, 2024-01-02, 10d
  Stop :s, 2024-01-05, until j
  Loop a :la, after lb, 1d
  Loop b :lb, after la, 1d
";
        let expected = [
            ("2024-01-12", "2024-01-14"), // after the later of Task-1 (Jan 4) and B2 (Jan 12)
            ("2024-01-01", "2024-01-04"),
            ("2024-01-02", "2024-01-12"),
            ("2024-01-05", "2024-01-12"), // until j starts
            ("2024-01-12", "2024-01-13"), // reference cycle: after the previous task
            ("2024-01-13", "2024-01-14"),
        ];
        let spans = spans_of(source);
        assert_eq!(spans.len(), expected.len());
        for (i, ((start, end), (want_start, want_end))) in spans.iter().zip(expected).enumerate() {
            assert_eq!(*start, day(want_start), "task {i} start");
            assert_eq!(*end, day(want_end), "task {i} end");
        }
    }

    #[test]
    fn charts_without_dates_count_days_from_zero() {
        let source = "gantt\n  A :a, 4d\n  B :b, 4d\n";
        let schedule = schedule_of(source);
        assert!(!schedule.has_dates);
        assert_eq!(
            schedule.spans.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
            [(0.0, 4.0), (4.0, 8.0)]
        );
        let labels: Vec<String> = layout_of(source).ticks.into_iter().map(|t| t.label).collect();
        assert_eq!(labels, ["0", "2", "4", "6", "8"]);
    }

    #[test]
    fn empty_chart_has_a_one_day_axis() {
        let layout = layout_of("gantt\n  title Nothing\n");
        assert_eq!((layout.time_start, layout.time_end), (0.0, 1.0));
        assert!(layout.tasks.is_empty());
    }
}
