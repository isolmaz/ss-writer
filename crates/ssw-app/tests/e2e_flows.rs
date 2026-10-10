//! End-to-end flows: `SSWriter.exe --test-hidden --test-profile <dir> --test-script <file>`.
//!
//! Every test launches the real executable against a temporary profile; nothing is shown on
//! the user's screen and no data outside the temporary folder is touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

struct Flow {
    dir: tempfile::TempDir,
}

impl Flow {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn settings(&self, json: &str) {
        let dir = self.path("profile").join("roaming").join("SS Writer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), json).unwrap();
    }

    /// Launches the executable with `script`; `Ok(())` when it exited with 0.
    fn launch(&self, script: &str) -> Output {
        let file = self.write("script.txt", script.as_bytes());
        Command::new(env!("CARGO_BIN_EXE_SSWriter"))
            .arg("--test-hidden")
            .arg("--test-profile")
            .arg(self.path("profile"))
            .arg("--test-script")
            .arg(file)
            .output()
            .unwrap()
    }

    fn run(&self, script: &str) {
        let output = self.launch(script);
        assert!(
            output.status.success(),
            "exit {:?}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Runs `script` followed by `dump-state` and returns the state.
    fn state(&self, script: &str) -> Value {
        let json = self.path("state.json");
        self.run(&format!("{script}\ndump-state {}\n", q(&json)));
        serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap()
    }
}

fn q(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

#[test]
fn enter_continues_lists_tasks_and_leaves_them_on_an_empty_item() {
    let flow = Flow::new();
    flow.run(
        "new\n\
         type - one\\n\n\
         assert-text-is - one\\n- \n\
         type two\\n\\n\n\
         assert-text-is - one\\n- two\\n\n\
         type - [ ] a\\n\n\
         assert-text-is - one\\n- two\\n- [ ] a\\n- [ ] \n",
    );
}

#[test]
fn enter_after_a_fence_opener_closes_the_fence() {
    let flow = Flow::new();
    flow.run(
        "new\n\
         type ```\\n\n\
         assert-text-is ```\\n\\n```\n\
         type code\n\
         assert-text-is ```\\ncode\\n```\n",
    );
}

#[test]
fn formatting_shortcuts_wrap_the_selection_and_undo_is_one_step() {
    let flow = Flow::new();
    flow.run(
        "new\n\
         type hello world\n\
         select-text world\n\
         key ctrl+b\n\
         assert-text-is hello **world**\n\
         key ctrl+z\n\
         assert-text-is hello world\n\
         assert-selection 6 11\n\
         command format.italic\n\
         assert-text-is hello *world*\n\
         key ctrl+z\n\
         assert-text-is hello world\n",
    );
}

#[test]
fn typing_pauses_start_new_undo_steps() {
    let flow = Flow::new();
    flow.run(
        "new\n\
         type one\n\
         advance 1500\n\
         type two\n\
         assert-text-is onetwo\n\
         key ctrl+z\n\
         assert-text-is one\n\
         key ctrl+z\n\
         assert-text-is \n",
    );
}

#[test]
fn saving_keeps_the_bom_and_the_line_endings_of_the_file() {
    let flow = Flow::new();
    let file = flow.write("crlf.md", b"\xEF\xBB\xBFa\r\nb\r\n");
    let expected = flow.write("expected.md", b"\xEF\xBB\xBFa\r\nb\r\nc");
    flow.settings(r#"{"general":{"autosave":false}}"#);
    let state = flow.state(&format!(
        "open {file}\nkey ctrl+end\ntype c\nassert-dirty true\nsave\nassert-dirty false\n\
         assert-file {file} {expected}\n",
        file = q(&file),
        expected = q(&expected),
    ));
    let tabs = state["windows"][0]["tabs"].as_array().unwrap();
    let tab = tabs
        .iter()
        .find(|tab| {
            tab["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("crlf.md"))
        })
        .unwrap();
    assert_eq!(tab["bom"], true);
    assert_eq!(tab["line_ending"], "Crlf");
    assert_eq!(tab["text"], "a\nb\nc");
}

#[test]
fn an_untitled_document_is_saved_through_the_save_dialog() {
    let flow = Flow::new();
    let out = flow.path("notes.md");
    let expected = flow.write("expected.md", b"# Notes\n");
    flow.run(&format!(
        "new\ntype # Notes\\n\nsave-as {out}\nassert-file {out} {expected}\nassert-dirty false\n",
        out = q(&out),
        expected = q(&expected),
    ));
}

#[test]
fn autosave_writes_after_the_delay_and_not_before() {
    let flow = Flow::new();
    flow.settings(r#"{"general":{"autosave_delay_ms":400}}"#);
    let file = flow.write("auto.md", b"start\n");
    let before = flow.write("before.md", b"start\n");
    let after = flow.write("after.md", b"start\nmore");
    flow.run(&format!(
        "open {file}\nkey ctrl+end\ntype more\nassert-file {file} {before}\nsleep 700\n\
         assert-dirty false\nassert-file {file} {after}\n",
        file = q(&file),
        before = q(&before),
        after = q(&after),
    ));
}

#[test]
fn an_external_change_reloads_a_clean_document() {
    let flow = Flow::new();
    let file = flow.write("watched.md", b"one\n");
    flow.run(&format!(
        "open {file}\nexternal-write {file} two\\nthree\\n\nwait-external 10000\n\
         assert-text-is two\\nthree\\n\nassert-dirty false\n",
        file = q(&file),
    ));
}

#[test]
fn an_external_change_to_a_document_with_edits_raises_the_conflict_banner() {
    let flow = Flow::new();
    flow.settings(r#"{"general":{"autosave":false}}"#);
    let file = flow.write("conflict.md", b"one\n");
    let state = flow.state(&format!(
        "open {file}\ntype mine\nexternal-write {file} theirs\\n\nwait-external 10000\n\
         assert-dirty true\n",
        file = q(&file),
    ));
    let window = &state["windows"][0];
    assert!(
        window["banners"]
            .as_array()
            .unwrap()
            .iter()
            .any(|banner| banner == "Conflict"),
        "{window}"
    );
}

#[test]
fn tabs_are_restored_by_the_next_launch_with_the_same_profile() {
    let flow = Flow::new();
    let a = flow.write("a.md", b"# A\n");
    let b = flow.write("b.txt", b"plain\n");
    flow.run(&format!("open {}\nopen {}\ntab 2\n", q(&a), q(&b)));
    let state = flow.state("");
    let paths: Vec<&str> = state["windows"][0]["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tab| tab["path"].as_str())
        .collect();
    assert!(paths.iter().any(|path| path.ends_with("a.md")), "{paths:?}");
    assert!(
        paths.iter().any(|path| path.ends_with("b.txt")),
        "{paths:?}"
    );
}

#[test]
fn a_failing_assertion_exits_non_zero_with_the_line() {
    let flow = Flow::new();
    let output = flow.launch("new\nassert-text-is something\n");
    assert_eq!(output.status.code(), Some(8));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("line 2"), "{stderr}");
    let invalid = flow.launch("frobnicate\n");
    assert_eq!(invalid.status.code(), Some(7));
}

#[test]
fn exporting_to_docx_writes_a_word_document() {
    let flow = Flow::new();
    let out = flow.path("out.docx");
    // The result report asks "open it, or show it in its folder?": the user shows the folder.
    flow.run(&format!(
        "new\ntype # Title\\n\\nBody\\n\nanswer save-file {out}\nanswer message-box no\n\
         command export.docx\nwait-idle\nassert-dialog message-box Export finished\n\
         assert-magic {out} docx\n",
        out = q(&out),
    ));
}
