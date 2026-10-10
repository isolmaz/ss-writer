//! "Markdown with attachments" export: writes the document and every local file it references
//! (images, linked files) into a target folder, rewriting the paths to point at the copies.
//!
//! Copies go to `attachments/<file name>` next to the document. Links to Markdown notes,
//! external URLs, anchors and files that do not exist are left untouched (missing files are
//! reported).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ssw_core::markdown::ast::{BlockKind, ImageKind, InlineKind, LinkKind, iter_inlines};
use ssw_core::markdown::parse_document;

use crate::assets::is_external_url;
use crate::html::percent_encode_path;

/// Folder (relative to the exported document) that receives the copied files.
pub const ATTACHMENTS_DIR: &str = "attachments";

#[derive(Debug, thiserror::Error)]
pub enum MarkdownExportError {
    #[error("invalid document file name: {0}")]
    InvalidFileName(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// What an export did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// The written Markdown file.
    pub markdown_path: PathBuf,
    /// Paths of the copied files relative to the target folder (`attachments/x.png`).
    pub attachments: Vec<String>,
    /// Destinations that looked local but did not name an existing file.
    pub missing: Vec<String>,
}

/// The rewritten Markdown and the files to copy: `(source file, relative target path)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPlan {
    pub markdown: String,
    pub copies: Vec<(PathBuf, String)>,
    pub missing: Vec<String>,
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    char::from(b).to_digit(16).map(|d| d as u8)
}

fn is_note(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".md", ".markdown", ".mdx"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// Byte range of the `src` attribute value inside an `<img …>` tag.
fn html_src_range(tag: &str) -> Option<(usize, usize)> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("src") {
        let at = from + found;
        from = at + 3;
        if at == 0 || !lower.as_bytes()[at - 1].is_ascii_whitespace() {
            continue;
        }
        let Some(rest) = tag[at + 3..].trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let value_start = tag.len() - rest.len() + 1;
        let value_len = tag[value_start..].find(quote)?;
        return Some((value_start, value_start + value_len));
    }
    None
}

/// Plans the export of `src` (a document living in `source_dir`).
pub fn plan(src: &str, source_dir: &Path) -> ExportPlan {
    let doc = parse_document(src);
    // (start, end, destination as resolved by the parser)
    let mut refs: Vec<(usize, usize, String)> = Vec::new();
    for block in doc.iter_blocks() {
        if let BlockKind::LinkReferenceDefinition(d) = &block.kind {
            refs.push((d.dest_range.start, d.dest_range.end, d.dest.clone()));
        }
        for group in block.inline_groups() {
            for node in iter_inlines(group) {
                match &node.kind {
                    InlineKind::Link(l) => {
                        if let LinkKind::Inline { dest_range, .. } = &l.kind {
                            refs.push((dest_range.start, dest_range.end, l.dest.clone()));
                        }
                    }
                    InlineKind::Image(i) => match &i.kind {
                        ImageKind::Inline { dest_range, .. } => {
                            refs.push((dest_range.start, dest_range.end, i.dest.clone()));
                        }
                        ImageKind::Html => {
                            let tag = node.range.get(src).unwrap_or("");
                            if let Some((s, e)) = html_src_range(tag) {
                                refs.push((
                                    node.range.start + s,
                                    node.range.start + e,
                                    i.dest.clone(),
                                ));
                            }
                        }
                        ImageKind::Reference { .. } => {}
                    },
                    _ => {}
                }
            }
        }
    }

    let mut targets: HashMap<PathBuf, String> = HashMap::new();
    let mut used_names: Vec<String> = Vec::new();
    let mut copies: Vec<(PathBuf, String)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (start, end, dest) in refs {
        if dest.is_empty() || dest.starts_with('#') || is_external_url(&dest) {
            continue;
        }
        let (path_part, suffix) = match dest.find('#') {
            Some(i) => (&dest[..i], &dest[i..]),
            None => (dest.as_str(), ""),
        };
        let decoded = percent_decode(path_part);
        if decoded.is_empty() || is_note(&decoded) {
            continue;
        }
        let candidate = Path::new(&decoded);
        let full = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            source_dir.join(candidate)
        };
        if !full.is_file() {
            if !missing.contains(&dest) {
                missing.push(dest.clone());
            }
            continue;
        }
        let rel = match targets.get(&full) {
            Some(rel) => rel.clone(),
            None => {
                let name = full
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let unique = unique_name(&name, &used_names);
                used_names.push(unique.to_lowercase());
                let rel = format!("{ATTACHMENTS_DIR}/{unique}");
                targets.insert(full.clone(), rel.clone());
                copies.push((full, rel.clone()));
                rel
            }
        };
        edits.push((start, end, format!("{}{suffix}", percent_encode_path(&rel))));
    }

    edits.sort_by_key(|edit| std::cmp::Reverse(edit.0));
    let mut markdown = src.to_string();
    for (start, end, text) in edits {
        markdown.replace_range(start..end, &text);
    }
    ExportPlan {
        markdown,
        copies,
        missing,
    }
}

/// `name`, or `stem-2.ext`, `stem-3.ext`… when a case-insensitive equal name is taken.
fn unique_name(name: &str, used_lowercase: &[String]) -> String {
    if !used_lowercase.contains(&name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut n = 2u32;
    loop {
        let candidate = format!("{stem}-{n}{ext}");
        if !used_lowercase.contains(&candidate.to_lowercase()) {
            return candidate;
        }
        n += 1;
    }
}

/// Writes `file_name` (e.g. `note.md`) with the rewritten Markdown into `target_dir` and copies
/// the referenced files into `target_dir/attachments/`.
pub fn export_to_folder(
    src: &str,
    source_dir: &Path,
    target_dir: &Path,
    file_name: &str,
) -> Result<ExportReport, MarkdownExportError> {
    if file_name.is_empty()
        || file_name == ".."
        || file_name == "."
        || file_name.contains(['/', '\\', ':', '\0'])
    {
        return Err(MarkdownExportError::InvalidFileName(file_name.to_string()));
    }
    let plan = plan(src, source_dir);
    std::fs::create_dir_all(target_dir)?;
    let mut attachments = Vec::with_capacity(plan.copies.len());
    for (from, rel) in &plan.copies {
        let to = target_dir.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, &to)?;
        attachments.push(rel.clone());
    }
    let markdown_path = target_dir.join(file_name);
    std::fs::write(&markdown_path, plan.markdown)?;
    Ok(ExportReport {
        markdown_path,
        attachments,
        missing: plan.missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("img")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("img/a b.png"), b"AB").unwrap();
        std::fs::write(root.join("img/c.png"), b"C1").unwrap();
        std::fs::write(root.join("other/c.png"), b"C2").unwrap();
        std::fs::write(root.join("report.pdf"), b"PDF").unwrap();
        tmp
    }

    #[test]
    fn rewrites_images_links_html_and_definitions() {
        let tmp = source_tree();
        let md = "![a](img/a%20b.png \"t\") [pdf](report.pdf#page=2) ![c](img/c.png) ![c2](other/c.png) ![again](img/c.png)\n\n<img src=\"img/c.png\" width=\"10\">\n\n[ref]: report.pdf\n\n![r][ref]";
        let plan = plan(md, tmp.path());
        let expected = "![a](attachments/a%20b.png \"t\") [pdf](attachments/report.pdf#page=2) ![c](attachments/c.png) ![c2](attachments/c-2.png) ![again](attachments/c.png)\n\n<img src=\"attachments/c.png\" width=\"10\">\n\n[ref]: attachments/report.pdf\n\n![r][ref]";
        assert_eq!(plan.markdown, expected);
        let rels: Vec<&str> = plan.copies.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(
            rels,
            [
                "attachments/a b.png",
                "attachments/report.pdf",
                "attachments/c.png",
                "attachments/c-2.png"
            ]
        );
        assert!(plan.missing.is_empty());
    }

    #[test]
    fn leaves_untouched_what_is_not_a_local_attachment() {
        let tmp = source_tree();
        let md = "[web](https://x.y/a.png) [note](other.md) [anchor](#top) [empty]() ![gone](nope.png) ![gone2](nope.png) <img alt=\"x\"> <img src=gone.png>";
        let plan = plan(md, tmp.path());
        assert_eq!(plan.markdown, md);
        assert!(plan.copies.is_empty());
        assert_eq!(plan.missing, ["nope.png"]);
    }

    #[test]
    fn absolute_paths_are_copied_too() {
        let tmp = source_tree();
        let abs = tmp.path().join("report.pdf");
        let md = format!("[x](<{}>)", abs.to_string_lossy().replace('\\', "/"));
        let elsewhere = tempfile::tempdir().unwrap();
        let plan = plan(&md, elsewhere.path());
        assert_eq!(plan.copies.len(), 1);
        assert!(
            plan.markdown.contains("attachments/report.pdf"),
            "{}",
            plan.markdown
        );
    }

    #[test]
    fn export_writes_document_and_copies() {
        let tmp = source_tree();
        let target = tempfile::tempdir().unwrap();
        let out = target.path().join("out");
        let report =
            export_to_folder("![a](img/c.png) ![m](nope.png)", tmp.path(), &out, "doc.md").unwrap();
        assert_eq!(report.markdown_path, out.join("doc.md"));
        assert_eq!(report.attachments, ["attachments/c.png"]);
        assert_eq!(report.missing, ["nope.png"]);
        assert_eq!(
            std::fs::read_to_string(out.join("doc.md")).unwrap(),
            "![a](attachments/c.png) ![m](nope.png)"
        );
        assert_eq!(std::fs::read(out.join("attachments/c.png")).unwrap(), b"C1");
    }

    #[test]
    fn invalid_file_names_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["", ".", "..", "a/b.md", "a\\b.md", "c:x.md"] {
            assert!(
                matches!(
                    export_to_folder("x", tmp.path(), tmp.path(), bad),
                    Err(MarkdownExportError::InvalidFileName(n)) if n == bad
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unique_names_are_case_insensitive_and_keep_extensions() {
        let used = vec![
            "a.png".to_string(),
            "a-2.png".to_string(),
            "noext".to_string(),
            ".hidden".to_string(),
        ];
        let table = [
            ("b.png", "b.png"),
            ("A.PNG", "A-3.PNG"),
            ("noext", "noext-2"),
            (".hidden", ".hidden-2"),
        ];
        for (name, expected) in table {
            assert_eq!(unique_name(name, &used), expected);
        }
    }

    #[test]
    fn percent_decoding_handles_invalid_and_multibyte_sequences() {
        let table = [
            ("a%20b", "a b"),
            ("%C3%BC", "ü"),
            ("100%", "100%"),
            ("%zz1", "%zz1"),
            ("%4", "%4"),
            ("plain", "plain"),
        ];
        for (input, expected) in table {
            assert_eq!(percent_decode(input), expected, "{input}");
        }
    }

    #[test]
    fn html_src_detection() {
        let table = [
            ("<img src=\"a.png\">", Some("a.png")),
            ("<img alt=x src='b c.png' width=1>", Some("b c.png")),
            ("<img data-src=\"no\" src = \"yes\">", Some("yes")),
            ("<img srcset=\"a 1x\" src=\"b.png\">", Some("b.png")),
            ("<img src=bare.png>", None),
            ("<img alt=\"x\">", None),
            ("<img src=\"unterminated>", None),
        ];
        for (tag, expected) in table {
            let got = html_src_range(tag).map(|(s, e)| &tag[s..e]);
            assert_eq!(got, expected, "{tag}");
        }
    }
}
