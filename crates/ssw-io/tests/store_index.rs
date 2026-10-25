//! The workspace index: walking with .gitignore / hidden rules, headings, links and backlinks,
//! incremental updates, limits, placeholders and cancellation.

use std::cell::Cell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rusqlite::Connection;
use ssw_io::index::{IndexChange, IndexOptions, IndexStats, IndexedHeading, MAX_PARSE_BYTES};
use ssw_io::store::{Store, StoreError};
use tempfile::{TempDir, tempdir};

const NOW: i64 = 1_700_000_000;

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    store: Store,
}

fn path_of(root: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = path_of(root, rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn no_placeholders(_: &Path) -> bool {
    false
}

fn run(store: &mut Store, root: &Path, opts: &IndexOptions) -> IndexStats {
    store
        .index_workspace(root, opts, &no_placeholders, &AtomicBool::new(false), NOW)
        .unwrap()
}

fn rels(store: &Store, root: &Path) -> Vec<String> {
    store
        .index_files(root)
        .unwrap()
        .into_iter()
        .map(|file| file.rel)
        .collect()
}

fn workspace(files: &[(&str, &str)]) -> Fixture {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    for (rel, text) in files {
        write(&root, rel, text);
    }
    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    Fixture {
        _tmp: tmp,
        root,
        store,
    }
}

const README: &str = "# Readme\n\nSee [[Notes/Plan]] and [guide](docs/guide.md#install).\n";
const PLAN: &str = "# Plan\n\n## Phase 1\n\nBack to [[readme]].\n";
const GUIDE: &str = "# Guide\n\n## Install\n\n![logo](../img/logo.png)\n\nSee [[Plan#Phase 1|the plan]] or [[#Install]].\n";

fn standard() -> Fixture {
    workspace(&[
        (".gitignore", "ignored/\n*.log\nsecret.md\n"),
        ("readme.md", README),
        ("Notes/Plan.md", PLAN),
        ("docs/guide.md", GUIDE),
        ("img/logo.png", "not really a png"),
        ("ignored/skip.md", "# Skipped\n"),
        ("debug.log", "log line\n"),
        ("secret.md", "# Secret\n"),
        (".hidden/h.md", "# Hidden\n"),
        (".git/config", "[core]\n"),
    ])
}

#[test]
fn walk_honors_gitignore_hidden_items_and_never_enters_git() {
    let mut fx = standard();
    let stats = run(&mut fx.store, &fx.root, &IndexOptions::default());
    assert_eq!(
        rels(&fx.store, &fx.root),
        [
            "docs/guide.md",
            "img/logo.png",
            "Notes/Plan.md",
            "readme.md"
        ]
    );
    assert_eq!(
        stats,
        IndexStats {
            files: 4,
            parsed: 3,
            removed: 0,
            placeholders: 0,
            truncated: false
        }
    );

    let files = fx.store.index_files(&fx.root).unwrap();
    let guide = &files[0];
    assert_eq!(guide.path, path_of(&fx.root, "docs/guide.md"));
    assert_eq!(guide.size, GUIDE.len() as u64);
    assert!(guide.markdown && !guide.placeholder);
    assert!(guide.modified_ms > 0);
    assert!(!files[1].markdown, "png is listed but not Markdown");

    // Hidden items on: dot files appear, ignored files and .git still do not.
    let shown = IndexOptions {
        show_hidden: true,
        ..IndexOptions::default()
    };
    run(&mut fx.store, &fx.root, &shown);
    assert_eq!(
        rels(&fx.store, &fx.root),
        [
            ".gitignore",
            ".hidden/h.md",
            "docs/guide.md",
            "img/logo.png",
            "Notes/Plan.md",
            "readme.md"
        ]
    );
}

#[test]
fn files_ignored_by_a_nested_gitignore_are_skipped() {
    let mut fx = workspace(&[
        ("keep.md", "# Keep\n"),
        ("sub/.gitignore", "drafts/\n*.tmp\n!important.tmp\n"),
        ("sub/note.md", "# Note\n"),
        ("sub/drafts/d.md", "# Draft\n"),
        ("sub/x.tmp", "t"),
        ("sub/important.tmp", "t"),
    ]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    assert_eq!(
        rels(&fx.store, &fx.root),
        ["keep.md", "sub/important.tmp", "sub/note.md"]
    );
}

#[test]
fn rerunning_only_reads_what_changed() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    run(&mut fx.store, &fx.root, &opts);

    let again = run(&mut fx.store, &fx.root, &opts);
    assert_eq!((again.parsed, again.removed, again.files), (0, 0, 4));

    // A different size is a change (modification time alone may be too coarse).
    write(
        &fx.root,
        "Notes/Plan.md",
        "# Plan v2\n\n## Phase 1\n\n## Phase 2\n",
    );
    std::fs::remove_file(path_of(&fx.root, "img/logo.png")).unwrap();
    write(&fx.root, "new.md", "# New\n");
    let changed = run(&mut fx.store, &fx.root, &opts);
    assert_eq!((changed.parsed, changed.removed, changed.files), (2, 1, 4));
    let plan = path_of(&fx.root, "Notes/Plan.md");
    let headings: Vec<String> = fx
        .store
        .index_headings(&fx.root, &plan)
        .unwrap()
        .into_iter()
        .map(|h| h.text)
        .collect();
    assert_eq!(headings, ["Plan v2", "Phase 1", "Phase 2"]);

    // A new ignore rule drops files that were indexed before.
    write(&fx.root, ".gitignore", "new.md\n");
    let ignored = run(&mut fx.store, &fx.root, &opts);
    assert_eq!(ignored.removed, 1);
    assert!(!rels(&fx.store, &fx.root).contains(&"new.md".to_owned()));
}

#[test]
fn headings_come_with_level_plain_text_and_offset() {
    let mut fx = workspace(&[
        ("plan.md", PLAN),
        (
            "fmt.md",
            "# Title with **bold** and `code`\n\ntext\n\nSetext-looking\n======\n\n### Closed ###\n",
        ),
        ("big.txt", "# not markdown\n"),
    ]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    assert_eq!(
        fx.store
            .index_headings(&fx.root, &path_of(&fx.root, "plan.md"))
            .unwrap(),
        vec![
            IndexedHeading {
                level: 1,
                text: "Plan".to_owned(),
                offset: 0
            },
            IndexedHeading {
                level: 2,
                text: "Phase 1".to_owned(),
                offset: 8
            },
        ]
    );
    let fmt = fx
        .store
        .index_headings(&fx.root, &path_of(&fx.root, "fmt.md"))
        .unwrap();
    assert_eq!(fmt[0].text, "Title with bold and code");
    // Only ATX headings exist (the parser has no Setext headings); the closing run is no text.
    assert_eq!(fmt.len(), 2);
    assert_eq!((fmt[1].level, fmt[1].text.as_str()), (3, "Closed"));

    // Lookups are case-insensitive; unknown, non-Markdown and foreign paths are just empty.
    let upper = fx
        .store
        .index_headings(&fx.root, &path_of(&fx.root, "PLAN.MD"))
        .unwrap();
    assert_eq!(upper.len(), 2);
    for other in [
        path_of(&fx.root, "missing.md"),
        path_of(&fx.root, "big.txt"),
        PathBuf::from("Z:\\elsewhere\\plan.md"),
    ] {
        assert!(
            fx.store
                .index_headings(&fx.root, &other)
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        fx.store
            .index_headings(Path::new("Z:\\unknown-root"), &path_of(&fx.root, "plan.md"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn backlinks_follow_wikilinks_markdown_links_and_images() {
    let mut fx = standard();
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let at = |rel: &str| path_of(&fx.root, rel);

    // `[[Notes/Plan]]` in readme and `[[Plan#Phase 1|the plan]]` in the guide.
    assert_eq!(
        fx.store
            .index_backlinks(&fx.root, &at("Notes/Plan.md"))
            .unwrap(),
        [at("docs/guide.md"), at("readme.md")]
    );
    // `[[readme]]` finds the file without its extension and folder.
    assert_eq!(
        fx.store
            .index_backlinks(&fx.root, &at("readme.md"))
            .unwrap(),
        [at("Notes/Plan.md")]
    );
    // A relative Markdown link, case-insensitively.
    assert_eq!(
        fx.store
            .index_backlinks(&fx.root, &at("DOCS/Guide.md"))
            .unwrap(),
        [at("readme.md")]
    );
    // An image: `![logo](../img/logo.png)` from docs/.
    assert_eq!(
        fx.store
            .index_backlinks(&fx.root, &at("img/logo.png"))
            .unwrap(),
        [at("docs/guide.md")]
    );
    assert!(
        fx.store
            .index_backlinks(&fx.root, &at("nobody.md"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn link_sites_describe_what_a_rename_must_rewrite() {
    let mut fx = standard();
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let at = |rel: &str| path_of(&fx.root, rel);

    let sites = fx
        .store
        .index_links_to(&fx.root, &at("Notes/Plan.md"))
        .unwrap();
    assert_eq!(sites.len(), 2);
    let (guide, readme) = (&sites[0], &sites[1]);

    assert_eq!(guide.source, at("docs/guide.md"));
    assert!(guide.wiki);
    assert_eq!(guide.target, "Plan");
    assert_eq!(guide.heading.as_deref(), Some("Phase 1"));
    assert_eq!(&GUIDE[guide.range.start..guide.range.end], "Plan");

    assert_eq!(readme.source, at("readme.md"));
    assert!(readme.wiki);
    assert_eq!(readme.target, "Notes/Plan");
    assert_eq!(readme.heading, None);
    assert_eq!(&README[readme.range.start..readme.range.end], "Notes/Plan");

    let guide_sites = fx
        .store
        .index_links_to(&fx.root, &at("docs/guide.md"))
        .unwrap();
    assert_eq!(guide_sites.len(), 1);
    assert!(!guide_sites[0].wiki);
    assert_eq!(guide_sites[0].target, "docs/guide.md#install");
    assert_eq!(guide_sites[0].heading.as_deref(), Some("install"));
    let range = guide_sites[0].range;
    assert!(README[range.start..range.end].starts_with("docs/guide.md"));

    // Unknown root: nothing.
    assert!(
        fx.store
            .index_links_to(Path::new("Z:\\unknown-root"), &at("readme.md"))
            .unwrap()
            .is_empty()
    );
    assert!(
        fx.store
            .index_links_to(&fx.root, Path::new("Z:\\elsewhere\\readme.md"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn markdown_link_destinations_are_resolved_like_a_reader_would() {
    let links = "\
[a](a.md) [b](./sub/../sub/b.md#top) [n](My%20Note.md) [abs](/abs.md) [q](a.md?x=1)
[web](https://example.com/a.md) [mail](mailto:a@example.com) [drive](C:\\a.md)
[anchor](#top) [cdn](//cdn.example.com/a.md) [esc](../../outside.md)

![img](pics/p.png)

[ref]: ref-target.md
";
    let mut fx = workspace(&[
        ("links.md", links),
        ("deep/inner.md", "[up](../a.md) [esc](../../x.md)\n"),
    ]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let at = |rel: &str| path_of(&fx.root, rel);
    let count = |rel: &str| fx.store.index_links_to(&fx.root, &at(rel)).unwrap().len();

    // a.md: `[a](a.md)`, `[q](a.md?x=1)` and `[up](../a.md)` from deep/; the URL, the
    // scheme-relative link and the drive path do not count.
    assert_eq!(count("a.md"), 3);
    let b = fx.store.index_links_to(&fx.root, &at("sub/b.md")).unwrap();
    assert_eq!((b.len(), b[0].heading.as_deref()), (1, Some("top")));
    assert_eq!(count("My Note.md"), 1);
    assert_eq!(count("abs.md"), 1);
    assert_eq!(count("pics/p.png"), 1);
    assert_eq!(count("ref-target.md"), 1);
    assert_eq!(count("outside.md"), 0);
    assert_eq!(count("x.md"), 0);
}

/// How many link rows the index holds.
fn link_rows(fx: &Fixture) -> i64 {
    Connection::open(fx._tmp.path().join("state.db"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM index_links", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn escapes_schemes_and_unusual_targets_are_resolved_like_a_reader_would() {
    let links = "\
[lower](caf%c3%a9.md) [upper](CAF%C3%A9.md) [bare](100%.md) [short](a%2) [bad](b%zz.md)

[plus](git+ssh:foo.md) [dash](my-app:foo.md) [dot](a.b:foo.md) [under](a_b:foo.md)

[here](.) [root](/) [none](LICENSE) [dotfile](.env)
";
    let mut fx = workspace(&[("links.md", links)]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let at = |rel: &str| path_of(&fx.root, rel);
    let count = |rel: &str| fx.store.index_links_to(&fx.root, &at(rel)).unwrap().len();

    // Escapes decode in either case of the hex digits; a `%` that starts no escape stays.
    assert_eq!(count("café.md"), 2);
    assert_eq!(count("100%.md"), 1);
    assert_eq!(count("a%2"), 1);
    assert_eq!(count("b%zz.md"), 1);
    // `+`, `-` and `.` belong to a scheme (these are not files), `_` does not.
    assert_eq!(count("foo.md"), 0);
    assert_eq!(count("a_b:foo.md"), 1);
    // Targets without an extension, and dot-files, match under their whole name.
    assert_eq!(count("LICENSE"), 1);
    assert_eq!(count(".env"), 1);
    // `.` and `/` point at no file, so they are not stored at all.
    assert_eq!(link_rows(&fx), 8);
}

#[test]
fn reference_links_autolinks_and_html_images_are_not_file_links() {
    let src = "\
[text][ref] [ref] ![alt][ref] <https://example.com/a.md> www.example.com/b.md x <img src=\"pics/h.png\" alt=\"h\"> y

[ref]: target.md
";
    let mut fx = workspace(&[("note.md", src)]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let at = |rel: &str| path_of(&fx.root, rel);
    // Only the definition names a file; the links that use it, the autolinks and the HTML
    // image do not.
    assert_eq!(
        fx.store
            .index_links_to(&fx.root, &at("target.md"))
            .unwrap()
            .len(),
        1
    );
    assert!(
        fx.store
            .index_links_to(&fx.root, &at("pics/h.png"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(link_rows(&fx), 1);
}

#[test]
fn wikilinks_match_by_path_suffix_without_extension_and_case() {
    let mut fx = workspace(&[
        ("Notes/Plan.md", "# Plan\n"),
        ("Other/Plan.md", "# Other plan\n"),
        ("img/logo.png", "png"),
        (
            "hub.md",
            "[[Notes/Plan]] [[notes\\PLAN.md]] [[Other/Plan]] [[Plan]] [[logo.png]] [[ ]] [[#local]]\n",
        ),
    ]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let at = |rel: &str| path_of(&fx.root, rel);
    let targets = |rel: &str| -> Vec<String> {
        fx.store
            .index_links_to(&fx.root, &at(rel))
            .unwrap()
            .into_iter()
            .map(|site| site.target)
            .collect()
    };
    // Folder-qualified links pick their file; the bare name is ambiguous and matches both.
    assert_eq!(
        targets("Notes/Plan.md"),
        ["Notes/Plan", "notes\\PLAN.md", "Plan"]
    );
    assert_eq!(targets("Other/Plan.md"), ["Other/Plan", "Plan"]);
    assert_eq!(targets("img/logo.png"), ["logo.png"]);
}

#[test]
fn a_page_does_not_count_as_its_own_backlink() {
    let mut fx = workspace(&[
        ("self.md", "[[self]] and [me](self.md)\n"),
        ("other.md", "[[self]]\n"),
    ]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let own = path_of(&fx.root, "self.md");
    assert_eq!(fx.store.index_links_to(&fx.root, &own).unwrap().len(), 3);
    assert_eq!(
        fx.store.index_backlinks(&fx.root, &own).unwrap(),
        [path_of(&fx.root, "other.md")]
    );
}

#[test]
fn file_events_update_the_index_incrementally() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    run(&mut fx.store, &fx.root, &opts);
    let at = |rel: &str| path_of(&fx.root, rel);
    let apply = |store: &mut Store, changes: &[IndexChange]| {
        store
            .index_apply_changes(&fx.root, changes, &opts, &no_placeholders)
            .unwrap()
    };

    // A new file that links to the readme.
    write(&fx.root, "Notes/New.md", "# New\n\n[[readme]]\n");
    let stats = apply(&mut fx.store, &[IndexChange::Changed(at("Notes/New.md"))]);
    assert_eq!((stats.files, stats.parsed), (5, 1));
    assert_eq!(
        fx.store
            .index_backlinks(&fx.root, &at("readme.md"))
            .unwrap(),
        [at("Notes/New.md"), at("Notes/Plan.md")]
    );

    // A modified file replaces its headings and links.
    write(&fx.root, "Notes/New.md", "# Renamed heading\n");
    apply(&mut fx.store, &[IndexChange::Changed(at("Notes/New.md"))]);
    assert_eq!(
        fx.store
            .index_headings(&fx.root, &at("Notes/New.md"))
            .unwrap()[0]
            .text,
        "Renamed heading"
    );
    assert_eq!(
        fx.store
            .index_backlinks(&fx.root, &at("readme.md"))
            .unwrap(),
        [at("Notes/Plan.md")]
    );

    // A deleted file (event, and gone from disk).
    std::fs::remove_file(at("Notes/New.md")).unwrap();
    let stats = apply(&mut fx.store, &[IndexChange::Removed(at("Notes/New.md"))]);
    assert_eq!((stats.files, stats.removed), (4, 1));

    // A path that vanished before we looked is dropped even when the event says "changed".
    let stats = apply(
        &mut fx.store,
        &[
            IndexChange::Changed(at("readme.md")),
            IndexChange::Removed(at("readme.md")),
        ],
    );
    assert_eq!(stats.removed, 1);
    std::fs::remove_file(at("docs/guide.md")).unwrap();
    let stats = apply(&mut fx.store, &[IndexChange::Changed(at("docs/guide.md"))]);
    assert_eq!(stats.removed, 1);
    assert_eq!(rels(&fx.store, &fx.root), ["img/logo.png", "Notes/Plan.md"]);
}

#[test]
fn folder_events_cover_everything_below() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    run(&mut fx.store, &fx.root, &opts);
    let at = |rel: &str| path_of(&fx.root, rel);

    write(&fx.root, "extra/a.md", "# A\n");
    write(&fx.root, "extra/sub/b.md", "# B\n");
    let stats = fx
        .store
        .index_apply_changes(
            &fx.root,
            &[IndexChange::Changed(at("extra"))],
            &opts,
            &no_placeholders,
        )
        .unwrap();
    assert_eq!((stats.files, stats.parsed), (6, 2));

    // A folder rename is a removal of the old folder, then a change of the new one.
    std::fs::rename(at("extra"), at("moved")).unwrap();
    let stats = fx
        .store
        .index_apply_changes(
            &fx.root,
            &[
                IndexChange::Removed(at("extra")),
                IndexChange::Changed(at("moved")),
            ],
            &opts,
            &no_placeholders,
        )
        .unwrap();
    assert_eq!((stats.files, stats.removed, stats.parsed), (6, 2, 2));
    let all = rels(&fx.store, &fx.root);
    assert!(all.contains(&"moved/sub/b.md".to_owned()));
    assert!(!all.iter().any(|rel| rel.starts_with("extra")));

    std::fs::remove_dir_all(at("moved")).unwrap();
    let stats = fx
        .store
        .index_apply_changes(
            &fx.root,
            &[IndexChange::Removed(at("moved"))],
            &opts,
            &no_placeholders,
        )
        .unwrap();
    assert_eq!((stats.files, stats.removed), (4, 2));
}

#[test]
fn events_for_ignored_hidden_foreign_or_unindexed_paths_change_nothing() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    run(&mut fx.store, &fx.root, &opts);
    let at = |rel: &str| path_of(&fx.root, rel);

    write(&fx.root, "ignored/new.md", "# Ignored\n");
    write(&fx.root, ".hidden/new.md", "# Hidden\n");
    write(&fx.root, ".git/new.md", "# Git\n");
    let outside = fx._tmp.path().join("outside.md");
    std::fs::write(&outside, "# Outside\n").unwrap();
    let stats = fx
        .store
        .index_apply_changes(
            &fx.root,
            &[
                IndexChange::Changed(at("ignored/new.md")),
                IndexChange::Changed(at(".hidden/new.md")),
                IndexChange::Changed(at(".git/new.md")),
                IndexChange::Changed(outside),
            ],
            &opts,
            &no_placeholders,
        )
        .unwrap();
    assert_eq!((stats.files, stats.parsed, stats.removed), (4, 0, 0));

    // With hidden items on, the hidden file is accepted (but .git never is).
    let shown = IndexOptions {
        show_hidden: true,
        ..IndexOptions::default()
    };
    let stats = fx
        .store
        .index_apply_changes(
            &fx.root,
            &[
                IndexChange::Changed(at(".hidden/new.md")),
                IndexChange::Changed(at(".git/new.md")),
            ],
            &shown,
            &no_placeholders,
        )
        .unwrap();
    assert_eq!((stats.files, stats.parsed), (5, 1));

    // A root that was never indexed is left alone.
    let other = fx._tmp.path().join("other-root");
    std::fs::create_dir_all(&other).unwrap();
    let stats = fx
        .store
        .index_apply_changes(
            &other,
            &[IndexChange::Changed(other.join("x.md"))],
            &opts,
            &no_placeholders,
        )
        .unwrap();
    assert_eq!(stats, IndexStats::default());
}

#[test]
fn the_file_limit_applies_to_the_walk_and_to_events() {
    let mut fx = standard();
    let limited = IndexOptions {
        show_hidden: false,
        max_files: 2,
    };
    let stats = run(&mut fx.store, &fx.root, &limited);
    assert_eq!((stats.files, stats.truncated), (2, true));

    let mut fx = standard();
    let opts = IndexOptions {
        show_hidden: false,
        max_files: 4,
    };
    let stats = run(&mut fx.store, &fx.root, &opts);
    assert!(!stats.truncated, "exactly at the limit is not truncated");
    write(&fx.root, "extra.md", "# Extra\n");
    let stats = fx
        .store
        .index_apply_changes(
            &fx.root,
            &[
                IndexChange::Changed(path_of(&fx.root, "extra.md")),
                // Known files are still updated at the limit.
                IndexChange::Changed(path_of(&fx.root, "readme.md")),
            ],
            &opts,
            &no_placeholders,
        )
        .unwrap();
    assert!(stats.truncated);
    assert_eq!((stats.files, stats.parsed), (4, 1));
    assert!(!rels(&fx.store, &fx.root).contains(&"extra.md".to_owned()));
}

#[test]
fn events_for_files_beyond_the_limit_do_not_probe_or_read_them() {
    let mut fx = standard();
    let opts = IndexOptions {
        show_hidden: false,
        max_files: 5,
    };
    run(&mut fx.store, &fx.root, &opts);
    for name in ["one", "two", "three"] {
        write(&fx.root, &format!("{name}.md"), &format!("# {name}\n"));
    }
    // The placeholder probe comes before the read of every file that is stored.
    let probed = Cell::new(0);
    let counting = |_: &Path| {
        probed.set(probed.get() + 1);
        false
    };
    let changes: Vec<IndexChange> = ["one", "two", "three"]
        .iter()
        .map(|name| IndexChange::Changed(path_of(&fx.root, &format!("{name}.md"))))
        .collect();
    let stats = fx
        .store
        .index_apply_changes(&fx.root, &changes, &opts, &counting)
        .unwrap();
    assert_eq!(probed.get(), 1, "only the file that still fits is read");
    assert!(stats.truncated);
    assert_eq!((stats.files, stats.parsed), (5, 1));
    let rels = rels(&fx.store, &fx.root);
    assert!(rels.contains(&"one.md".to_owned()));
    assert!(!rels.contains(&"two.md".to_owned()) && !rels.contains(&"three.md".to_owned()));
}

#[test]
fn a_new_file_pending_at_the_limit_is_stored_again_by_its_next_event() {
    let mut fx = standard();
    let opts = IndexOptions {
        show_hidden: false,
        max_files: 5,
    };
    run(&mut fx.store, &fx.root, &opts);
    write(&fx.root, "one.md", "# One\n");
    let probed = Cell::new(0);
    let counting = |_: &Path| {
        probed.set(probed.get() + 1);
        false
    };
    // The second event finds the file pending, not new: it still fits and is read again.
    let one = path_of(&fx.root, "one.md");
    let changes = [IndexChange::Changed(one.clone()), IndexChange::Changed(one)];
    let stats = fx
        .store
        .index_apply_changes(&fx.root, &changes, &opts, &counting)
        .unwrap();
    assert_eq!(probed.get(), 2);
    assert!(!stats.truncated);
    assert_eq!(stats.files, 5);
    assert!(rels(&fx.store, &fx.root).contains(&"one.md".to_owned()));
}

#[test]
fn placeholders_are_listed_but_never_read() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    let guide = path_of(&fx.root, "docs/guide.md");
    let is_placeholder = |path: &Path| path == guide;
    let stats = fx
        .store
        .index_workspace(
            &fx.root,
            &opts,
            &is_placeholder,
            &AtomicBool::new(false),
            NOW,
        )
        .unwrap();
    assert_eq!((stats.files, stats.parsed, stats.placeholders), (4, 2, 1));
    let files = fx.store.index_files(&fx.root).unwrap();
    assert!(files[0].placeholder && files[0].markdown);
    assert!(
        fx.store
            .index_headings(&fx.root, &guide)
            .unwrap()
            .is_empty()
    );

    // Once hydrated it is read.
    let stats = run(&mut fx.store, &fx.root, &opts);
    assert_eq!((stats.parsed, stats.placeholders), (1, 0));
    assert_eq!(fx.store.index_headings(&fx.root, &guide).unwrap().len(), 2);

    // The same through a file event.
    let stats = fx
        .store
        .index_apply_changes(
            &fx.root,
            &[IndexChange::Changed(guide.clone())],
            &opts,
            &is_placeholder,
        )
        .unwrap();
    assert_eq!((stats.parsed, stats.placeholders), (0, 1));
    assert!(
        fx.store
            .index_headings(&fx.root, &guide)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn oversized_and_unreadable_markdown_is_listed_without_being_parsed() {
    let mut fx = workspace(&[("small.md", "# Small\n")]);
    let big = path_of(&fx.root, "big.md");
    std::fs::write(&big, vec![b'x'; MAX_PARSE_BYTES as usize + 1]).unwrap();
    write(&fx.root, "locked.md", "# Locked\n");
    let locked = path_of(&fx.root, "locked.md");
    // No sharing at all: reading the file fails while the walk can still see it.
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked)
        .unwrap();

    let stats = run(&mut fx.store, &fx.root, &IndexOptions::default());
    assert_eq!((stats.files, stats.parsed), (3, 1));
    let files = fx.store.index_files(&fx.root).unwrap();
    let listed: Vec<(&str, bool)> = files.iter().map(|f| (f.rel.as_str(), f.markdown)).collect();
    assert_eq!(
        listed,
        [("big.md", true), ("locked.md", true), ("small.md", true)]
    );
    assert!(fx.store.index_headings(&fx.root, &big).unwrap().is_empty());
    assert!(
        fx.store
            .index_headings(&fx.root, &locked)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn invalid_utf8_markdown_is_decoded_leniently() {
    let mut fx = workspace(&[]);
    std::fs::write(path_of(&fx.root, "bad.md"), b"# Caf\xE9 title\n").unwrap();
    let stats = run(&mut fx.store, &fx.root, &IndexOptions::default());
    assert_eq!(stats.parsed, 1);
    let headings = fx
        .store
        .index_headings(&fx.root, &path_of(&fx.root, "bad.md"))
        .unwrap();
    assert_eq!(headings[0].text, "Caf\u{FFFD} title");
}

#[test]
fn cancellation_leaves_the_previous_index_intact() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    let cancel = AtomicBool::new(true);
    let error = fx
        .store
        .index_workspace(&fx.root, &opts, &no_placeholders, &cancel, NOW)
        .unwrap_err();
    assert!(matches!(error, StoreError::Cancelled));
    assert!(error.to_string().contains("cancelled"));
    assert!(fx.store.index_files(&fx.root).unwrap().is_empty());

    run(&mut fx.store, &fx.root, &opts);
    for (rel, text) in [
        ("readme.md", "# Changed readme\n"),
        ("Notes/Plan.md", "# Changed plan\n"),
        ("docs/guide.md", "# Changed guide\n"),
    ] {
        write(&fx.root, rel, text);
    }
    // The predicate is called per file before any row of the batch is written; it cancels the
    // run.
    let cancel = AtomicBool::new(false);
    let cancelling = |_: &Path| {
        cancel.store(true, Ordering::Relaxed);
        false
    };
    let error = fx
        .store
        .index_workspace(&fx.root, &opts, &cancelling, &cancel, NOW)
        .unwrap_err();
    assert!(matches!(error, StoreError::Cancelled));
    let headings = fx
        .store
        .index_headings(&fx.root, &path_of(&fx.root, "readme.md"))
        .unwrap();
    assert_eq!(
        headings[0].text, "Readme",
        "the cancelled run was rolled back"
    );
}

/// A second connection to the database of `fx` that fails at once instead of waiting for a lock.
fn impatient(fx: &Fixture) -> Connection {
    let conn = Connection::open(fx._tmp.path().join("state.db")).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    conn
}

#[test]
fn the_database_stays_writable_while_files_are_read_and_parsed() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    let other = impatient(&fx);
    let writes = Cell::new(0);
    // The predicate runs for every file before its rows are written, as the file is read and
    // parsed: the interface's own writes (recent files, view state, history) must go through.
    let writing = |_: &Path| {
        other
            .execute(
                "INSERT OR REPLACE INTO kv (key, value) VALUES ('ui', 'wrote')",
                [],
            )
            .expect("the index run holds the write lock while it reads files");
        writes.set(writes.get() + 1);
        false
    };

    let stats = fx
        .store
        .index_workspace(&fx.root, &opts, &writing, &AtomicBool::new(false), NOW)
        .unwrap();
    assert_eq!((stats.files, stats.parsed, writes.get()), (4, 3, 4));

    write(&fx.root, "Notes/New.md", "# New\n\n[[readme]]\n");
    write(&fx.root, "Notes/Newer.md", "# Newer\n");
    writes.set(0);
    let changes = [
        IndexChange::Changed(path_of(&fx.root, "Notes/New.md")),
        IndexChange::Changed(path_of(&fx.root, "Notes/Newer.md")),
    ];
    let stats = fx
        .store
        .index_apply_changes(&fx.root, &changes, &opts, &writing)
        .unwrap();
    assert_eq!((stats.files, stats.parsed, writes.get()), (6, 2, 2));
    assert_eq!(fx.store.kv_get("ui").unwrap().as_deref(), Some("wrote"));
}

#[test]
fn a_run_over_many_files_writes_them_all_in_several_transactions() {
    let mut fx = workspace(&[]);
    let opts = IndexOptions::default();
    let note = |i: usize| format!("notes/n{i:03}.md");
    for i in 0..600 {
        write(
            &fx.root,
            &note(i),
            &format!("# Note {i}\n\nSee [[n{:03}]].\n", (i + 1) % 600),
        );
    }
    let stats = run(&mut fx.store, &fx.root, &opts);
    assert_eq!((stats.files, stats.parsed, stats.removed), (600, 600, 0));
    assert_eq!(link_rows(&fx), 600);
    let heading = |store: &Store, i: usize| {
        let found = store
            .index_headings(&fx.root, &path_of(&fx.root, &note(i)))
            .unwrap();
        found.into_iter().map(|h| h.text).collect::<Vec<_>>()
    };
    assert_eq!(heading(&fx.store, 599), ["Note 599"]);

    // Changed files are re-read, vanished ones dropped, the rest is left alone.
    for i in 0..300 {
        write(&fx.root, &note(i), &format!("# Note {i} changed\n"));
    }
    for i in 300..450 {
        std::fs::remove_file(path_of(&fx.root, &note(i))).unwrap();
    }
    let stats = run(&mut fx.store, &fx.root, &opts);
    assert_eq!((stats.files, stats.parsed, stats.removed), (450, 300, 150));
    assert_eq!(link_rows(&fx), 150);
    assert_eq!(heading(&fx.store, 0), ["Note 0 changed"]);
    assert_eq!(heading(&fx.store, 299), ["Note 299 changed"]);
    assert_eq!(heading(&fx.store, 599), ["Note 599"]);
    assert!(heading(&fx.store, 300).is_empty());
}

#[test]
fn many_events_are_applied_in_several_transactions() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    run(&mut fx.store, &fx.root, &opts);
    let apply = |store: &mut Store, changes: &[IndexChange]| {
        store
            .index_apply_changes(&fx.root, changes, &opts, &no_placeholders)
            .unwrap()
    };
    let batch = |i: usize| path_of(&fx.root, &format!("batch/b{i:03}.md"));
    for i in 0..300 {
        write(
            &fx.root,
            &format!("batch/b{i:03}.md"),
            &format!("# Batch {i}\n"),
        );
    }

    // One event per file: the steps pending between two events are written first.
    let changes: Vec<IndexChange> = (0..300).map(|i| IndexChange::Changed(batch(i))).collect();
    let stats = apply(&mut fx.store, &changes);
    assert_eq!((stats.files, stats.parsed, stats.removed), (304, 300, 0));

    // One event for a folder: the steps pending between two of its files are written first.
    for i in 0..300 {
        write(
            &fx.root,
            &format!("folder/f{i:03}.md"),
            &format!("# Folder {i}\n"),
        );
    }
    let stats = apply(
        &mut fx.store,
        &[IndexChange::Changed(path_of(&fx.root, "folder"))],
    );
    assert_eq!((stats.files, stats.parsed, stats.removed), (604, 300, 0));

    // Removals count as steps too.
    for i in 0..300 {
        std::fs::remove_file(batch(i)).unwrap();
    }
    let changes: Vec<IndexChange> = (0..300).map(|i| IndexChange::Removed(batch(i))).collect();
    let stats = apply(&mut fx.store, &changes);
    assert_eq!((stats.files, stats.parsed, stats.removed), (304, 0, 300));
    let all = rels(&fx.store, &fx.root);
    assert!(all.contains(&"folder/f299.md".to_owned()));
    assert!(!all.iter().any(|rel| rel.starts_with("batch")));
}

#[test]
fn forgetting_a_root_drops_its_index_only() {
    let mut fx = standard();
    let opts = IndexOptions::default();
    run(&mut fx.store, &fx.root, &opts);
    let second = fx._tmp.path().join("second");
    write(&second, "only.md", "# Only\n[[readme]]\n");
    run(&mut fx.store, &second, &opts);

    // Roots do not see each other's files or links.
    assert_eq!(rels(&fx.store, &second), ["only.md"]);
    assert!(
        fx.store
            .index_backlinks(&fx.root, &path_of(&fx.root, "readme.md"))
            .unwrap()
            .contains(&path_of(&fx.root, "Notes/Plan.md"))
    );
    assert!(
        !fx.store
            .index_backlinks(&fx.root, &path_of(&fx.root, "readme.md"))
            .unwrap()
            .contains(&path_of(&second, "only.md"))
    );

    fx.store.index_forget(&fx.root).unwrap();
    assert!(fx.store.index_files(&fx.root).unwrap().is_empty());
    assert!(
        fx.store
            .index_links_to(&fx.root, &path_of(&fx.root, "readme.md"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(rels(&fx.store, &second), ["only.md"]);
    // Forgetting again is fine.
    fx.store.index_forget(&fx.root).unwrap();
}

#[test]
fn unrepresentable_paths_are_rejected_everywhere() {
    let mut fx = workspace(&[("a.md", "# A\n")]);
    run(&mut fx.store, &fx.root, &IndexOptions::default());
    let bad = PathBuf::from(OsString::from_wide(&[0x43, 0x3A, 0x5C, 0xD800]));
    let good = path_of(&fx.root, "a.md");
    let rejected = |error: StoreError| matches!(error, StoreError::NonUnicodePath(_));
    let opts = IndexOptions::default();

    assert!(rejected(
        fx.store
            .index_workspace(&bad, &opts, &no_placeholders, &AtomicBool::new(false), NOW)
            .unwrap_err()
    ));
    assert!(rejected(
        fx.store
            .index_apply_changes(&bad, &[], &opts, &no_placeholders)
            .unwrap_err()
    ));
    assert!(rejected(
        fx.store
            .index_apply_changes(
                &fx.root,
                &[IndexChange::Changed(bad.clone())],
                &opts,
                &no_placeholders
            )
            .unwrap_err()
    ));
    assert!(rejected(fx.store.index_forget(&bad).unwrap_err()));
    assert!(rejected(fx.store.index_files(&bad).unwrap_err()));
    assert!(rejected(fx.store.index_headings(&bad, &good).unwrap_err()));
    assert!(rejected(
        fx.store.index_headings(&fx.root, &bad).unwrap_err()
    ));
    assert!(rejected(fx.store.index_links_to(&bad, &good).unwrap_err()));
    assert!(rejected(
        fx.store.index_links_to(&fx.root, &bad).unwrap_err()
    ));
    assert!(rejected(
        fx.store.index_backlinks(&fx.root, &bad).unwrap_err()
    ));
}
