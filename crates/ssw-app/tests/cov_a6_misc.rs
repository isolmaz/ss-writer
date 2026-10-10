//! Behaviour of small pieces of the UI layer that need no window: the Mermaid diagram menu, a
//! file-drop clipboard that carries no files, and links to files that have no path.

use std::path::{Path, PathBuf};

use ssw_app::i18n::localizer_for;
use ssw_app::ui::clipboard::{AttachmentTarget, Paste, plan_paste, relative_destination};
use ssw_app::ui::context_menus::{Action, mermaid_menu};
use ssw_app::ui::export::DiagramFormat;
use ssw_core::keymap::Keymap;
use ssw_core::settings::Attachments;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::services::{Clipboard, ClipboardContent, ClipboardFormat, ServiceError};
use ssw_win::menus::MenuEntry;

#[test]
fn the_mermaid_menu_offers_png_and_svg_for_the_diagram_it_was_opened_on() {
    let i18n = localizer_for("en");
    let anchor = Rect::new(40.0, 100.0, 0.0, 0.0);
    let popup = mermaid_menu("graph TD\n  A-->B\n", anchor, &i18n, &Keymap::default());

    assert_eq!(popup.at, Point::new(40.0, 100.0));
    let labels: Vec<&str> = popup
        .model
        .entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Item(item) => item.label.as_str(),
            other => panic!("only items expected, found {other:?}"),
        })
        .collect();
    let png = i18n.text("media-mermaid-export-png");
    let svg = i18n.text("media-mermaid-export-svg");
    assert_eq!(labels, [png.as_str(), svg.as_str()]);

    let ids = popup.ids();
    assert_eq!(ids.len(), 2);
    for (id, expected) in ids.iter().zip([DiagramFormat::Png, DiagramFormat::Svg]) {
        match popup.action(*id) {
            Some(Action::MermaidExport { source, format }) => {
                assert_eq!(source, "graph TD\n  A-->B\n");
                assert_eq!(
                    std::mem::discriminant(format),
                    std::mem::discriminant(&expected)
                );
            }
            other => panic!("item {id} should export the diagram, found {other:?}"),
        }
    }
    assert!(
        popup.action(0).is_none(),
        "ids the menu does not contain do nothing"
    );
}

/// The file-drop format with nothing in it, next to text (a drop list that was emptied).
struct EmptyDrop;

impl Clipboard for EmptyDrop {
    fn set(&mut self, _: ClipboardContent) -> Result<(), ServiceError> {
        Ok(())
    }

    fn clear(&mut self) -> Result<(), ServiceError> {
        Ok(())
    }

    fn formats(&self) -> Result<Vec<ClipboardFormat>, ServiceError> {
        Ok(vec![ClipboardFormat::Files, ClipboardFormat::Text])
    }

    fn text(&self) -> Result<Option<String>, ServiceError> {
        Ok(Some("typed text".to_owned()))
    }

    fn html(&self) -> Result<Option<String>, ServiceError> {
        Ok(None)
    }

    fn rtf(&self) -> Result<Option<String>, ServiceError> {
        Ok(None)
    }

    fn image(&self) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(None)
    }

    fn files(&self) -> Result<Vec<PathBuf>, ServiceError> {
        Ok(Vec::new())
    }

    fn custom(&self, _: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(None)
    }
}

#[test]
fn a_file_drop_without_files_pastes_the_text_next_to_it() {
    assert!(matches!(plan_paste(&EmptyDrop), Ok(Paste::Text)));
}

#[test]
fn a_target_without_path_components_has_no_relative_destination() {
    assert_eq!(
        relative_destination(Path::new("C:\\docs"), Path::new("")),
        None
    );
    assert_eq!(
        relative_destination(Path::new(""), Path::new("C:\\docs\\a.png")),
        None
    );
    let target = AttachmentTarget::new(Path::new("C:\\docs\\a.md"), &Attachments::default());
    assert_eq!(target.destination(Path::new("")), "");
}
