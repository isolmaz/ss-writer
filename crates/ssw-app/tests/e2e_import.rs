//! End-to-end: word-processor files open as Markdown documents imported on a worker (spec
//! §4.1), through File → Open of the real executable with scripted dialogs. The documents are
//! third-party fixtures (`tests/fixtures/SOURCES.md`).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(relative)
}

fn q(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// Runs `script` (then a state dump) in a fresh profile; returns the exit code, stderr, state.
fn launch(dir: &Path, script: &str) -> (Option<i32>, String, Option<Value>) {
    let file = dir.join("script.txt");
    let state = dir.join("state.json");
    std::fs::write(&file, format!("{script}\ndump-state {}\n", q(&state))).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_SSWriter"))
        .arg("--test-hidden")
        .arg("--test-profile")
        .arg(dir.join("profile"))
        .arg("--test-script")
        .arg(&file)
        .output()
        .unwrap();
    let json = std::fs::read_to_string(&state)
        .ok()
        .map(|text| serde_json::from_str(&text).unwrap());
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        json,
    )
}

/// The tab of the imported file, from the state of the active window.
fn tab_of<'a>(state: &'a Value, name: &str) -> &'a Value {
    state["windows"][0]["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tab| {
            tab["path"]
                .as_str()
                .is_some_and(|path| path.ends_with(name))
        })
        .unwrap_or_else(|| panic!("no tab for {name} in {state}"))
}

fn open_script(path: &Path) -> String {
    format!(
        "answer open-file {}\ncommand file.open\nwait-idle\n",
        q(path)
    )
}

#[test]
fn word_processor_files_open_as_markdown_documents() {
    for name in [
        "docx/mammoth_simple-list.docx",
        "odt/LibreOfficeWriter_odt_1.3.odt",
        "rtf/testRTFBoldItalic.rtf",
        "doc/simple-list.doc",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(name);
        assert!(path.is_file(), "{}", path.display());
        let (code, stderr, state) = launch(dir.path(), &open_script(&path));
        assert_eq!(code, Some(0), "{name}: {stderr}");
        let state = state.unwrap();
        let file_name = name.rsplit('/').next().unwrap();
        let tab = tab_of(&state, file_name);
        let text = tab["text"].as_str().unwrap();
        assert_eq!(tab["mode"], "Markdown", "{name}");
        assert!(!text.trim().is_empty(), "{name} imported nothing");
        for raw in ["<w:", "{\\rtf", "<office:", "PK\u{3}\u{4}"] {
            assert!(
                !text.contains(raw),
                "{name} shows the raw container: {text}"
            );
        }
        assert_eq!(
            tab["dirty"], false,
            "{name}: a fresh import has no unsaved edits"
        );
        assert_eq!(
            state["shell"].as_array().unwrap().len(),
            0,
            "{name} was handed to the default application"
        );
        assert_eq!(
            state["dialogs"].as_array().unwrap().len(),
            1,
            "{name}: only the open dialog"
        );
    }
}

#[test]
fn what_the_import_could_not_represent_raises_the_loss_banner() {
    let dir = tempfile::tempdir().unwrap();
    let (code, stderr, state) = launch(
        dir.path(),
        &open_script(&fixture("docx/mammoth_comments.docx")),
    );
    assert_eq!(code, Some(0), "{stderr}");
    let state = state.unwrap();
    let tab = tab_of(&state, "mammoth_comments.docx");
    assert!(
        tab["losses"].as_u64().unwrap() > 0,
        "comments are reported as lost"
    );
    let banners = state["windows"][0]["banners"].as_array().unwrap();
    assert!(
        banners.iter().any(|banner| banner == "ImportLoss"),
        "{banners:?}"
    );
}

#[test]
fn an_imported_document_is_saved_as_markdown_and_the_original_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture("docx/mammoth_simple-list.docx");
    let before = std::fs::read(&source).unwrap();
    let out = dir.path().join("converted.md");
    let script = format!(
        "{}key ctrl+end\ntype  more\nassert-dirty true\nsave-as {}\nassert-dirty false\n",
        open_script(&source),
        q(&out)
    );
    let (code, stderr, state) = launch(dir.path(), &script);
    assert_eq!(code, Some(0), "{stderr}");
    let state = state.unwrap();
    let tab = tab_of(&state, "converted.md");
    let written = std::fs::read_to_string(&out).unwrap();
    assert_eq!(written, tab["text"].as_str().unwrap());
    assert!(written.ends_with(" more"), "{written}");
    assert_eq!(
        std::fs::read(&source).unwrap(),
        before,
        "the source file is never written"
    );
}

#[test]
fn files_the_importer_refuses_are_reported_and_open_nothing() {
    for name in ["doc/PasswordProtected.doc", "doc/Word6.doc"] {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(name);
        let script = format!("answer message-box ok\n{}", open_script(&path));
        let (code, stderr, state) = launch(dir.path(), &script);
        assert_eq!(code, Some(0), "{name}: {stderr}");
        let state = state.unwrap();
        let dialogs = state["dialogs"].as_array().unwrap();
        let message = dialogs
            .iter()
            .find(|dialog| dialog["kind"] == "message-box")
            .unwrap_or_else(|| panic!("{name}: the failure was not reported: {state}"));
        assert_eq!(message["answered"], true);
        assert!(!message["message"].as_str().unwrap().is_empty());
        let tabs = state["windows"][0]["tabs"].as_array().unwrap();
        assert!(
            tabs.iter().all(|tab| tab["path"].is_null()),
            "{name} opened a tab: {state}"
        );
    }
}
