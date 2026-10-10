//! `cargo xtask screenshots`: builds the release executable, writes the demo documents and one
//! scenario script per screenshot set below `target/screens`, runs
//! `SSWriter.exe --test-hidden --test-profile <fresh profile> --test-script <script>` for every
//! scenario in the light and the dark theme (nothing is ever shown on screen: the `screenshot`
//! steps of the script harness paint the real window content offscreen) and copies the PNG files
//! into `docs/screenshots/`.

use std::path::{Path, PathBuf};
use std::process::Command;

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

use crate::cli::ScreenshotArgs;
use crate::error::Error;
use crate::fsx;
use crate::metadata::Metadata;
use crate::run;
use crate::workspace::Workspace;

/// The themes every scenario runs in.
pub const THEMES: [&str; 2] = ["light", "dark"];

const BANNER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="260" viewBox="0 0 1200 260">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="#1874D3"/><stop offset="1" stop-color="#7B5CF0"/></linearGradient></defs>
<rect width="1200" height="260" fill="url(#g)"/>
<circle cx="980" cy="60" r="150" fill="#ffffff" fill-opacity="0.12"/>
<circle cx="1100" cy="220" r="110" fill="#ffffff" fill-opacity="0.10"/>
<circle cx="140" cy="230" r="90" fill="#ffffff" fill-opacity="0.10"/>
<path d="M0 190 C200 120 400 250 600 180 S1000 110 1200 170 L1200 260 L0 260 Z" fill="#ffffff" fill-opacity="0.16"/>
</svg>"##;

/// The rich demo document of the overview screenshots.
pub const OVERVIEW: &str = "# Project Atlas: launch plan

![Atlas launch](images/banner.png)

Atlas is our **single source of truth** for the spring launch. It keeps the *schedule*, the `risk log` and the [release checklist](https://example.com/checklist) in one place, and every ==decision== is written down here.

> [!NOTE] Status
> The beta is feature complete. Only polish and documentation are left.

## Goals

- Ship the public beta by **April 14**
- Keep the crash rate below 0.1 %
  - Triage every report within a day
  - Fix regressions before new features

1. Freeze the feature set
2. Run the full regression suite
3. Publish the release notes

## Checklist

- [x] Finalize the roadmap
- [x] Book the launch venue
- [ ] Record the demo video
- [ ] Send the press kit

> [!WARNING] Risk
> The installer signing certificate expires on April 10. Renew it first.

## Budget

| Item | Owner | Cost | Status |
| :--- | :---- | ---: | :----: |
| Venue | Maya | 4,200 | paid |
| Video | Jonas | 2,800 | open |
| Swag | Priya | 1,150 | ordered |

## Performance model

Latency grows with the queue length $n$ as $T(n) = T_0 + \\tfrac{n}{\\mu}$, and the throughput bound is

$$
\\lambda_{\\max} = \\frac{1}{T_0 + \\sum_{i=1}^{k} \\frac{s_i}{\\mu_i}}
$$

## Release flow

```mermaid
graph LR
  A[Plan] --> B[Build]
  B --> C{Tests pass?}
  C -->|yes| D[Release]
  C -->|no| B
```

## Code

```rust
fn launch(day: u32) -> Result<(), Error> {
    let plan = Plan::load(\"atlas.toml\")?;
    plan.run(day)
}
```
";

const TABLE_DOC: &str = "# Fruit stand

| Fruit | Price | Stock |
| :---- | ----: | ----: |
| Apple | 0.80 | 120 |
| Banana | 0.45 | 80 |
| Cherry | 4.20 | 15 |

Prices are per piece, stock is counted every morning.
";

const TODO_TXT: &str = "Groceries
  oat milk
  coffee beans
  lemons

Call the dentist on Monday.
Renew the library card.
Water the plants (Tuesday and Friday).
";

const ARCHITECTURE_MMD: &str = "flowchart LR
  Client[Browser] --> Edge[Edge cache]
  Edge --> API{API gateway}
  API --> Auth[Auth service]
  API --> Orders[Order service]
  Orders --> DB[(Database)]
  Auth --> DB
";

const RELEASE_NOTES: &str = "# Release notes

Version 1.0 is the first public release.

- Live Markdown editing
- Folders, tabs and quick open
";

/// A short note for the sidebar demo.
fn note(title: &str, body: &str) -> String {
    format!("# {title}\n\n{body}\n")
}

/// Writes the demo documents below `dir`.
pub fn write_fixtures(dir: &Path) -> Result<(), Error> {
    fsx::write_file(&dir.join("overview.md"), OVERVIEW)?;
    fsx::write_file(&dir.join("table.md"), TABLE_DOC)?;
    fsx::write_file(&dir.join("plain").join("todo.txt"), TODO_TXT)?;
    fsx::write_file(
        &dir.join("diagrams").join("architecture.mmd"),
        ARCHITECTURE_MMD,
    )?;
    fsx::write_file(&dir.join("history").join("release-notes.md"), RELEASE_NOTES)?;
    fsx::write_file(&dir.join("images").join("banner.png"), banner_png()?)?;
    let notes = dir.join("notes");
    for (path, title, body) in [
        (
            "README.md",
            "Notes",
            "Everything I want to remember, one file per topic.",
        ),
        (
            "meeting-notes.md",
            "Weekly sync",
            "- Beta scope is frozen\n- Maya owns the venue\n- Next sync on Thursday",
        ),
        ("ideas.txt", "Ideas", "A dark mode for the website."),
        (
            "projects/atlas.md",
            "Atlas",
            "The spring launch. See [[meeting-notes]] for the latest status.",
        ),
        (
            "projects/orion.md",
            "Orion",
            "Research spike for the sync engine.",
        ),
        (
            "journal/2026-03-02.md",
            "Monday",
            "Planning day. Wrote the launch outline.",
        ),
        (
            "journal/2026-03-09.md",
            "Monday",
            "Demo rehearsal went well.",
        ),
    ] {
        let text = if path.ends_with(".txt") {
            body.to_owned()
        } else {
            note(title, body)
        };
        fsx::write_file(&notes.join(path), text)?;
    }
    Ok(())
}

/// The banner image of the overview document.
fn banner_png() -> Result<Vec<u8>, Error> {
    let invalid = |what: String| Error::Invalid(format!("banner image: {what}"));
    let tree = usvg::Tree::from_str(BANNER_SVG, &usvg::Options::default())
        .map_err(|error| invalid(error.to_string()))?;
    let mut pixmap = Pixmap::new(1200, 260).ok_or_else(|| invalid("no pixmap".to_owned()))?;
    resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());
    pixmap
        .encode_png()
        .map_err(|error| invalid(error.to_string()))
}

/// One screenshot set: the script lines after `theme` and the PNG names (without the theme
/// suffix) the script writes.
#[derive(Debug)]
pub struct Scenario {
    /// The scenario name (`--only`).
    pub name: &'static str,
    /// The steps after the `theme` line.
    pub steps: Vec<String>,
    /// The shots the steps take.
    pub shots: Vec<&'static str>,
}

fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// Every scenario for documents in `fx`, writing PNGs below `out` as `<shot>-<theme>.png`
/// at `scale_percent`.
pub fn scenarios(fx: &Path, out: &Path, theme: &str, scale_percent: u32) -> Vec<Scenario> {
    let scale = format!("{}", scale_percent as f32 / 100.0);
    let shot = |name: &str| {
        format!(
            "screenshot {} 1440 900 {scale}",
            quoted(&out.join(format!("{name}-{theme}.png")))
        )
    };
    let settings = |name: &str| {
        format!(
            "screenshot-settings {} 900 640 {scale}",
            quoted(&out.join(format!("{name}-{theme}.png")))
        )
    };
    let file = |relative: &str| quoted(&fx.join(relative));
    let notes = quoted(&fx.join("notes"));
    let strong = OVERVIEW.find("single source of truth").unwrap_or(0) + "single source".len();
    let table_cell = TABLE_DOC.find("Banana").unwrap_or(0) + 3;
    let s = |lines: &[String]| lines.to_vec();
    vec![
        Scenario {
            name: "editor-overview",
            steps: s(&[
                format!("open {}", file("overview.md")),
                "wait-idle".into(),
                shot("editor-overview"),
            ]),
            shots: vec!["editor-overview"],
        },
        Scenario {
            name: "editor-reveal",
            steps: s(&[
                format!("open {}", file("overview.md")),
                "wait-idle".into(),
                format!("select {strong} {strong}"),
                shot("editor-reveal"),
            ]),
            shots: vec!["editor-reveal"],
        },
        Scenario {
            name: "sidebar-workspace",
            steps: s(&[
                format!("open {}", file("notes/meeting-notes.md")),
                format!("answer pick-folder {notes}"),
                "command file.open_folder".into(),
                "command view.toggle_inspector".into(),
                "wait-idle".into(),
                shot("sidebar-workspace"),
            ]),
            shots: vec!["sidebar-workspace"],
        },
        Scenario {
            name: "find-replace",
            steps: s(&[
                format!("open {}", file("overview.md")),
                "command search.replace".into(),
                "type launch".into(),
                "key tab".into(),
                "type release".into(),
                shot("find-replace"),
            ]),
            shots: vec!["find-replace"],
        },
        Scenario {
            name: "quick-open",
            // The overview document fills the window below the overlay: a three-line note sits
            // entirely behind it (the panel covers the top of the text column).
            steps: s(&[
                format!("open {}", file("overview.md")),
                format!("answer pick-folder {notes}"),
                "command file.open_folder".into(),
                "command search.quick_open".into(),
                "wait-idle".into(),
                "type atl".into(),
                shot("quick-open"),
            ]),
            shots: vec!["quick-open"],
        },
        Scenario {
            name: "command-palette",
            steps: s(&[
                format!("open {}", file("overview.md")),
                "command search.command_palette".into(),
                "wait-idle".into(),
                "type export".into(),
                shot("command-palette"),
            ]),
            shots: vec!["command-palette"],
        },
        Scenario {
            name: "settings",
            steps: s(&[
                "command settings.open".into(),
                "settings-page appearance".into(),
                settings("settings-appearance"),
                "settings-page editor".into(),
                settings("settings-editor"),
            ]),
            shots: vec!["settings-appearance", "settings-editor"],
        },
        Scenario {
            name: "export-dialog",
            steps: s(&[
                format!("open {}", file("overview.md")),
                "command file.export".into(),
                shot("export-dialog"),
            ]),
            shots: vec!["export-dialog"],
        },
        Scenario {
            name: "version-history",
            steps: {
                let notes = file("history/release-notes.md");
                s(&[
                    format!("open {notes}"),
                    "type Release candidate 1\\n\\n".into(),
                    "save".into(),
                    "command tab.close".into(),
                    format!("open {notes}"),
                    "type Release candidate 2: fixes for the installer.\\n\\n".into(),
                    "save".into(),
                    "command tab.close".into(),
                    format!("open {notes}"),
                    "type Final edits before shipping.\\n\\n".into(),
                    "command file.version_history".into(),
                    "wait-idle".into(),
                    "key down".into(),
                    "wait-idle".into(),
                    shot("version-history"),
                ])
            },
            shots: vec!["version-history"],
        },
        Scenario {
            name: "plain-text",
            steps: s(&[
                format!("open {}", file("plain/todo.txt")),
                shot("plain-text"),
            ]),
            shots: vec!["plain-text"],
        },
        Scenario {
            name: "mermaid-document",
            steps: s(&[
                format!("open {}", file("diagrams/architecture.mmd")),
                "wait-idle".into(),
                shot("mermaid-document"),
            ]),
            shots: vec!["mermaid-document"],
        },
        Scenario {
            name: "table-editing",
            steps: s(&[
                format!("open {}", file("table.md")),
                format!("select {table_cell} {table_cell}"),
                "command table.insert_row_below".into(),
                "command table.align_right".into(),
                shot("table-editing"),
            ]),
            shots: vec!["table-editing"],
        },
        Scenario {
            name: "zen-focus",
            steps: s(&[
                format!("open {}", file("overview.md")),
                "command view.focus_mode".into(),
                "command view.zen_mode".into(),
                shot("zen-focus"),
            ]),
            shots: vec!["zen-focus"],
        },
    ]
}

/// The script text of `scenario` in `theme`.
pub fn script(scenario: &Scenario, theme: &str) -> String {
    let mut text = format!("# {} ({theme})\ntheme {theme}\n", scenario.name);
    for step in &scenario.steps {
        text.push_str(step);
        text.push('\n');
    }
    text
}

/// The folders of one scenario run: its own fresh copy of the demo documents (a run edits and
/// autosaves them) and its own fresh profile.
#[derive(Debug)]
pub struct RunDirs {
    /// The demo documents of this run.
    pub fixtures: PathBuf,
    /// The `--test-profile` folder of this run.
    pub profile: PathBuf,
}

/// Recreates `work/runs/<scenario>-<theme>` with fresh fixtures and an unused profile folder.
pub fn prepare_run(work: &Path, scenario: &str, theme: &str) -> Result<RunDirs, Error> {
    let root = work.join("runs").join(format!("{scenario}-{theme}"));
    fsx::remove_dir_if_exists(&root)?;
    let dirs = RunDirs {
        fixtures: root.join("fixtures"),
        profile: root.join("profile"),
    };
    write_fixtures(&dirs.fixtures)?;
    Ok(dirs)
}

/// Runs `cargo xtask screenshots`.
pub fn run(workspace: &Workspace, args: &ScreenshotArgs) -> Result<(), Error> {
    if !args.no_build {
        let mut build = run::cargo();
        build
            .args(["build", "--release", "--locked", "-p", "ssw-app"])
            .current_dir(&workspace.root);
        run::run(&mut build)?;
    }
    let metadata = Metadata::load(workspace)?;
    let exe = metadata
        .target_directory
        .join("release")
        .join("SSWriter.exe");
    let work: PathBuf = metadata.target_directory.join("screens");
    let out = work.join("out");
    fsx::remove_dir_if_exists(&out)?;
    fsx::create_dir(&out)?;
    let docs = workspace.path("docs/screenshots");
    let mut problems = Vec::new();
    for theme in THEMES {
        let names: Vec<&str> = scenarios(&work, &out, theme, args.scale_percent)
            .iter()
            .map(|scenario| scenario.name)
            .collect();
        for (index, name) in names.into_iter().enumerate() {
            if !args.only.is_empty() && !args.only.iter().any(|only| only == name) {
                continue;
            }
            println!("screenshots: {name} ({theme})");
            // The scripts name the run's own documents: nothing one run saves reaches another.
            let dirs = prepare_run(&work, name, theme)?;
            let scenario =
                scenarios(&dirs.fixtures, &out, theme, args.scale_percent).swap_remove(index);
            let script_path = work.join("scripts").join(format!("{name}-{theme}.txt"));
            fsx::write_file(&script_path, script(&scenario, theme))?;
            let mut command = Command::new(&exe);
            command
                .arg("--test-hidden")
                .arg("--test-profile")
                .arg(&dirs.profile)
                .arg("--test-script")
                .arg(&script_path);
            let output = run::capture(&mut command)?;
            if !output.status.success() {
                problems.push(format!(
                    "{} ({theme}) failed ({}); script: {}",
                    scenario.name,
                    output.status,
                    script_path.display()
                ));
                continue;
            }
            for shot in &scenario.shots {
                let name = format!("{shot}-{theme}.png");
                match fsx::read_bytes(&out.join(&name)) {
                    Ok(bytes) => fsx::write_file(&docs.join(&name), bytes)?,
                    Err(error) => problems.push(error.to_string()),
                }
            }
        }
    }
    if problems.is_empty() {
        println!("screenshots: {}", docs.display());
        Ok(())
    } else {
        Err(Error::Failed(problems.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_names_and_shots_are_unique_and_every_shot_is_taken_by_its_script() {
        let all = scenarios(Path::new("fx"), Path::new("out"), "dark", 100);
        let mut names: Vec<_> = all.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
        assert!(all.len() >= 13);
        let mut shots: Vec<_> = all.iter().flat_map(|s| s.shots.clone()).collect();
        let total = shots.len();
        shots.sort_unstable();
        shots.dedup();
        assert_eq!(shots.len(), total);
        for scenario in &all {
            let text = script(scenario, "dark");
            assert!(text.contains("\ntheme dark\n"), "{}", scenario.name);
            for shot in &scenario.shots {
                assert!(text.contains(&format!("{shot}-dark.png")), "{shot}");
            }
        }
    }

    #[test]
    fn the_scale_reaches_every_screenshot_step() {
        let all = scenarios(Path::new("fx"), Path::new("out"), "light", 200);
        let text = script(&all[0], "light");
        assert!(
            text.contains("editor-overview-light.png\" 1440 900 2\n"),
            "{text}"
        );
    }

    #[test]
    fn every_run_gets_its_own_fresh_fixtures_and_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let first = prepare_run(tmp.path(), "find-replace", "light").unwrap();
        let second = prepare_run(tmp.path(), "find-replace", "dark").unwrap();
        assert_ne!(first.fixtures, second.fixtures);
        assert_ne!(first.profile, second.profile);
        // A run edits (and autosaves) its documents and leaves files of its own behind.
        let edited = first.fixtures.join("overview.md");
        std::fs::write(&edited, "exportlaunch").unwrap();
        std::fs::write(first.fixtures.join("stray.md"), "x").unwrap();
        std::fs::create_dir_all(&first.profile).unwrap();
        let untouched = std::fs::read_to_string(second.fixtures.join("overview.md")).unwrap();
        assert_eq!(untouched, OVERVIEW);
        // Preparing the run again starts from the pristine documents and an unused profile.
        let again = prepare_run(tmp.path(), "find-replace", "light").unwrap();
        assert_eq!(again.fixtures, first.fixtures);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), OVERVIEW);
        assert!(!first.fixtures.join("stray.md").exists());
        assert!(!again.profile.exists());
    }

    #[test]
    fn the_scripts_of_a_run_open_the_documents_of_its_own_fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let run = prepare_run(tmp.path(), "quick-open", "light").unwrap();
        let all = scenarios(&run.fixtures, Path::new("out"), "light", 100);
        let quick_open = all.iter().find(|s| s.name == "quick-open").unwrap();
        let text = script(quick_open, "light");
        // The overview document is taller than the overlay: it stays visible behind it.
        let overview = quoted(&run.fixtures.join("overview.md"));
        assert!(text.contains(&format!("open {overview}\n")), "{text}");
        assert!(!text.contains("README.md"), "{text}");
    }

    #[test]
    fn the_fixtures_are_written_with_a_real_banner_image() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixtures(tmp.path()).unwrap();
        let overview = std::fs::read_to_string(tmp.path().join("overview.md")).unwrap();
        for needle in [
            "```mermaid",
            "$$",
            "> [!NOTE]",
            "- [x]",
            "| Item |",
            "```rust",
        ] {
            assert!(overview.contains(needle), "{needle}");
        }
        let banner = std::fs::read(tmp.path().join("images").join("banner.png")).unwrap();
        assert_eq!(&banner[..8], b"\x89PNG\r\n\x1a\n");
        assert!(
            tmp.path()
                .join("notes")
                .join("projects")
                .join("atlas.md")
                .is_file()
        );
        assert!(
            tmp.path()
                .join("diagrams")
                .join("architecture.mmd")
                .is_file()
        );
    }
}
