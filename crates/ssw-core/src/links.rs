//! Links, wikilinks and workspace paths (spec §3.2).
//!
//! * [`extract_links`] lists the links, images and wikilinks of a parsed document with their
//!   targets classified ([`LinkTarget`]).
//! * [`resolve_wikilink`] maps `[[name]]` onto a workspace file ([`RelPath`] list): exact path,
//!   path without extension, then the name as a trailing part of a path (with and without
//!   extension; a bare file stem matches anywhere). The shortest path wins, ties go to the
//!   alphabetically first. Comparison is case-insensitive and ordinal like NTFS
//!   (`CompareStringOrdinal`): characters are compared after Unicode simple uppercasing, so
//!   `ı` equals `I` but `İ` is not `i`.
//! * [`resolve_relative`] resolves Markdown link paths (percent-decoded, `./`, `../`) against the
//!   containing document and refuses anything that leaves the workspace.
//! * [`rename_edits`] rewrites the links that point at a renamed or moved file; [`backlinks`]
//!   lists the links pointing at a file; [`complete_wikilink`] and [`complete_heading`] provide
//!   the `[[` / `[[#` completion lists.
//! * [`is_inside_root`] is the lexical check behind "paths are canonicalized and rejected if they
//!   escape the root" for absolute Windows paths; [`wikilink_new_file`] maps a missing wikilink
//!   target to the workspace-relative path to create.
//!
//! Everything here is lexical: no function touches the file system (symbolic links and junctions
//! are the caller's concern).

use std::cmp::Ordering;
use std::collections::HashMap;

use thiserror::Error;

use crate::edit::Edit;
use crate::fuzzy::FuzzyMatcher;
use crate::markdown::ast::{BlockKind, Document, ImageKind, InlineKind, LinkKind, iter_inlines};
use crate::search::{fold_char, simple_upper};
use crate::text::TextRange;
use crate::toc::{Toc, slugify};

/// A path cannot be used as a workspace-relative path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathError {
    #[error("the path is empty")]
    Empty,
    #[error("the path is absolute")]
    Absolute,
    #[error("the path leaves the workspace root")]
    EscapesRoot,
    #[error("the path contains a name or character that Windows does not allow")]
    Invalid,
}

// ------------------------------------------------------------------------ ordinal comparison

/// `CompareStringOrdinal(…, ignoreCase = TRUE) == CSTR_EQUAL`: equal after Unicode simple
/// uppercasing of every character.
pub fn eq_ordinal_ignore_case(a: &str, b: &str) -> bool {
    a.chars().map(simple_upper).eq(b.chars().map(simple_upper))
}

/// Ordering of `CompareStringOrdinal(…, ignoreCase = TRUE)`: UTF-16 code units of the simply
/// uppercased strings.
pub fn cmp_ordinal_ignore_case(a: &str, b: &str) -> Ordering {
    let upper = |s: &str| s.chars().map(simple_upper).collect::<String>();
    upper(a).encode_utf16().cmp(upper(b).encode_utf16())
}

// ----------------------------------------------------------------------------- paths

/// A workspace-relative file path with `/` separators: no leading separator, no drive, no `.`
/// or `..` segments, no empty segments, and only names Windows accepts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    /// Normalizes `path`: `\` and `/` both separate segments, empty and `.` segments vanish and
    /// `..` removes the previous segment.
    ///
    /// Rejected: the empty path (also `.` and `a/..`), absolute paths (leading separator, drive
    /// letter `C:`, UNC), `..` that would leave the root, and segments containing control
    /// characters or `<>:"|?*`, ending in a dot or space, or naming a DOS device (`CON`,
    /// `NUL.txt`, `COM1`…).
    pub fn parse(path: &str) -> Result<RelPath, PathError> {
        if path.is_empty() {
            return Err(PathError::Empty);
        }
        if is_absolute_like(path) {
            return Err(PathError::Absolute);
        }
        let mut segments: Vec<&str> = Vec::new();
        for segment in path.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => {
                    if segments.pop().is_none() {
                        return Err(PathError::EscapesRoot);
                    }
                }
                name if segment_is_valid(name) => segments.push(name),
                _ => return Err(PathError::Invalid),
            }
        }
        if segments.is_empty() {
            Err(PathError::Empty)
        } else {
            Ok(RelPath(segments.join("/")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The segments, outermost first.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The last segment.
    pub fn file_name(&self) -> &str {
        self.0
            .rsplit_once('/')
            .map_or(self.0.as_str(), |(_, name)| name)
    }

    /// Everything before the last segment (`""` for a file in the root).
    pub fn parent_dir(&self) -> &str {
        self.0.rsplit_once('/').map_or("", |(dir, _)| dir)
    }

    /// Extension of the file name without the dot; `None` for `noext`, `.hidden` and `x.`.
    pub fn extension(&self) -> Option<&str> {
        self.file_name()
            .rsplit_once('.')
            .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
            .map(|(_, ext)| ext)
    }

    /// The path without the extension of its file name.
    pub fn without_extension(&self) -> &str {
        match self.extension() {
            Some(ext) => &self.0[..self.0.len() - ext.len() - 1],
            None => &self.0,
        }
    }

    /// `.md`, `.markdown` or `.mdx`.
    pub fn is_markdown(&self) -> bool {
        self.extension().is_some_and(is_markdown_extension)
    }
}

fn is_markdown_extension(ext: &str) -> bool {
    ["md", "markdown", "mdx"]
        .iter()
        .any(|m| ext.eq_ignore_ascii_case(m))
}

/// Leading separator (rooted, UNC) or drive letter (`C:`, also `C:name`).
fn is_absolute_like(path: &str) -> bool {
    path.starts_with(['/', '\\'])
        || matches!(path.as_bytes(), [d, b':', ..] if d.is_ascii_alphabetic())
}

/// A name Windows accepts for a file or folder (`.` and `..` are handled by the callers).
fn segment_is_valid(name: &str) -> bool {
    if name
        .chars()
        .any(|c| c < ' ' || c == '\u{7f}' || "<>:\"|?*".contains(c))
        || name.ends_with(['.', ' '])
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    !is_device_name(stem)
}

fn is_device_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|digit| matches!(digit.as_bytes(), [b'0'..=b'9']))
}

fn same_file(a: &RelPath, b: &RelPath) -> bool {
    eq_ordinal_ignore_case(a.as_str(), b.as_str())
}

/// Resolves the percent-decoded `link_path` of a Markdown link written in the document
/// `doc`: relative to the document's folder, `./` and `../` applied.
pub fn resolve_relative(doc: &RelPath, link_path: &str) -> Result<RelPath, PathError> {
    if link_path.is_empty() {
        return Err(PathError::Empty);
    }
    if is_absolute_like(link_path) {
        return Err(PathError::Absolute);
    }
    let dir = doc.parent_dir();
    if dir.is_empty() {
        RelPath::parse(link_path)
    } else {
        RelPath::parse(&format!("{dir}/{link_path}"))
    }
}

/// `to` written relative to the folder `from_dir` (`""` or `a/b`): `../` for every folder that
/// has to be left, then the remaining segments.
fn relative_path(from_dir: &str, to: &RelPath) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let to_dir: Vec<&str> = to
        .parent_dir()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let common = from
        .iter()
        .zip(&to_dir)
        .take_while(|(a, b)| eq_ordinal_ignore_case(a, b))
        .count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to_dir[common..]);
    parts.push(to.file_name());
    parts.join("/")
}

// ------------------------------------------------------------------ percent encoding

/// Decodes `%XX` escapes. Text that is not valid UTF-8 after decoding is returned unchanged,
/// and so is every `%` that is not followed by two hex digits.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: Option<&u8>| b.and_then(|&b| char::from(b).to_digit(16));
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2)))
        {
            decoded.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| text.to_owned())
}

/// Encodes a relative path for use as a link destination. `%`, `#`, `<`, `>` and control
/// characters are always escaped; spaces and parentheses too unless the destination is written
/// in `<…>` (`angle`). Other characters, including non-ASCII, stay readable.
fn encode_destination(path: &str, angle: bool) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        let escape = matches!(c, '%' | '#' | '<' | '>')
            || c.is_ascii_control()
            || (!angle && matches!(c, ' ' | '(' | ')'));
        if escape {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ------------------------------------------------------------------------ extraction

/// What a link points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// A URL with a scheme (`https:`, `mailto:`, `file:`…) or starting with `//`.
    External(String),
    /// A local reference of a Markdown link or image: percent-decoded `path` as written
    /// (empty for `#anchor` links) and the percent-decoded fragment.
    File {
        path: String,
        fragment: Option<String>,
    },
    /// `[[name]]`, `[[name#heading]]`, `[[#heading]]` (empty `name`); both trimmed.
    Wiki {
        name: String,
        heading: Option<String>,
    },
}

/// Which syntax a [`LinkRef`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkSource {
    Link,
    Image,
    Wiki,
}

/// One link, image or wikilink of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRef {
    pub source: LinkSource,
    /// The whole node.
    pub range: TextRange,
    pub target: LinkTarget,
    /// The destination as written: the inline destination or the wikilink target. `None` for
    /// reference-style links and `<img>` tags (nothing to rewrite at the use site).
    pub dest_range: Option<TextRange>,
}

/// Classifies a resolved link destination.
pub fn classify_destination(dest: &str) -> LinkTarget {
    if has_scheme(dest) || dest.starts_with("//") {
        return LinkTarget::External(dest.to_owned());
    }
    let (path, fragment) = match dest.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (dest, None),
    };
    LinkTarget::File {
        path: percent_decode(path),
        fragment: fragment.map(percent_decode),
    }
}

/// `scheme:` with a scheme of two or more characters (so `C:\x` is a path).
fn has_scheme(dest: &str) -> bool {
    let Some((scheme, _)) = dest.split_once(':') else {
        return false;
    };
    scheme.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// Every link, image and wikilink of `doc` in source order (table cells and headings
/// included), with its target.
pub fn extract_links(doc: &Document) -> Vec<LinkRef> {
    let mut links = Vec::new();
    for block in doc.iter_blocks() {
        for group in block.inline_groups() {
            for inline in iter_inlines(group) {
                let (source, target, dest_range) = match &inline.kind {
                    InlineKind::Link(link) => {
                        let dest_range = match &link.kind {
                            LinkKind::Inline { dest_range, .. } => Some(*dest_range),
                            LinkKind::Reference { .. } | LinkKind::Autolink { .. } => None,
                        };
                        (
                            LinkSource::Link,
                            classify_destination(&link.dest),
                            dest_range,
                        )
                    }
                    InlineKind::Image(image) => {
                        let dest_range = match &image.kind {
                            ImageKind::Inline { dest_range, .. } => Some(*dest_range),
                            ImageKind::Reference { .. } | ImageKind::Html => None,
                        };
                        (
                            LinkSource::Image,
                            classify_destination(&image.dest),
                            dest_range,
                        )
                    }
                    InlineKind::WikiLink(wiki) => {
                        let target = LinkTarget::Wiki {
                            name: wiki.target.trim().to_owned(),
                            heading: wiki.heading.as_deref().map(|h| h.trim().to_owned()),
                        };
                        (LinkSource::Wiki, target, Some(wiki.target_range))
                    }
                    _ => continue,
                };
                links.push(LinkRef {
                    source,
                    range: inline.range,
                    target,
                    dest_range,
                });
            }
        }
    }
    links
}

// ------------------------------------------------------------------------ resolution

/// The workspace file `[[name]]` points at, or `None`. `name` uses `/` or `\` and may omit the
/// extension and leading folders. Matching tiers, best first: the whole path; the whole path
/// without extension; a trailing part of the path (segment-aligned); a trailing part of the
/// path without extension (a bare file stem matches in any folder). Within a tier the shortest
/// path wins, then the alphabetically first (ordinal, ignoring case).
pub fn resolve_wikilink<'a>(files: &'a [RelPath], name: &str) -> Option<&'a RelPath> {
    let name = name.trim().replace('\\', "/");
    if name.is_empty() {
        return None;
    }
    files
        .iter()
        .filter_map(|file| wiki_tier(file, &name).map(|tier| (tier, file)))
        .min_by(|(tier_a, a), (tier_b, b)| {
            tier_a
                .cmp(tier_b)
                .then_with(|| a.as_str().chars().count().cmp(&b.as_str().chars().count()))
                .then_with(|| cmp_ordinal_ignore_case(a.as_str(), b.as_str()))
                .then_with(|| a.as_str().cmp(b.as_str()))
        })
        .map(|(_, file)| file)
}

/// How well `name` addresses `file`; lower is better, `None` when it does not.
fn wiki_tier(file: &RelPath, name: &str) -> Option<u8> {
    let full = file.as_str();
    let bare = file.without_extension();
    if eq_ordinal_ignore_case(full, name) {
        Some(0)
    } else if eq_ordinal_ignore_case(bare, name) {
        Some(1)
    } else if ends_with_segments(full, name) {
        Some(2)
    } else if ends_with_segments(bare, name) {
        Some(3)
    } else {
        None
    }
}

/// `path` ends with `name` on a `/` boundary (ordinal, ignoring case).
fn ends_with_segments(path: &str, name: &str) -> bool {
    let mut tail = path.chars().rev();
    for expected in name.chars().rev() {
        match tail.next() {
            Some(c) if simple_upper(c) == simple_upper(expected) => {}
            _ => return false,
        }
    }
    tail.next().is_none_or(|c| c == '/')
}

/// Index of the heading `wanted` (a `[[note#heading]]` fragment or `#anchor`) in `toc`: first
/// by title (ignoring case and the Turkish `i` variants, whitespace collapsed), then by anchor.
pub fn find_heading(toc: &Toc, wanted: &str) -> Option<usize> {
    let key = |text: &str| -> Vec<char> {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .map(fold_char)
            .collect()
    };
    let wanted_key = key(wanted);
    let by_title = toc
        .entries()
        .iter()
        .position(|e| key(&e.title) == wanted_key);
    by_title.or_else(|| {
        let slug = slugify(wanted.trim());
        toc.entries().iter().position(|e| e.slug == slug)
    })
}

/// The workspace-relative path to create for a wikilink to a missing note: `name` parsed as
/// a [`RelPath`] (so escapes and absolute paths are refused) with `.md` appended unless it
/// already ends in a document extension (`md`, `markdown`, `mdx`, `txt`, `mmd`).
pub fn wikilink_new_file(name: &str) -> Result<RelPath, PathError> {
    let path = RelPath::parse(name.trim())?;
    let is_document = path.extension().is_some_and(|ext| {
        is_markdown_extension(ext) || ["txt", "mmd"].iter().any(|d| ext.eq_ignore_ascii_case(d))
    });
    if is_document {
        Ok(path)
    } else {
        RelPath::parse(&format!("{}.md", path.as_str()))
    }
}

// ------------------------------------------------------------------------ rename

/// A place in a document where a destination is written and can be rewritten.
struct Site {
    target: LinkTarget,
    dest_range: TextRange,
}

fn sites(doc: &Document) -> Vec<Site> {
    let mut sites: Vec<Site> = extract_links(doc)
        .into_iter()
        .filter_map(|link| {
            link.dest_range.map(|dest_range| Site {
                target: link.target,
                dest_range,
            })
        })
        .collect();
    for block in doc.iter_blocks() {
        if let BlockKind::LinkReferenceDefinition(def) = &block.kind {
            sites.push(Site {
                target: classify_destination(&def.dest),
                dest_range: def.dest_range,
            });
        }
    }
    sites
}

/// Edits (sorted, disjoint, against `text`) that make the links of the document `doc_path`
/// follow the rename or move of `old_path` to `new_path`. `files` is the workspace file list
/// *before* the change; it decides which files a wikilink resolved to.
///
/// * Wikilinks keep their alias and heading. The name keeps its style: with or without
///   extension, and at least as many folder segments as before; it is lengthened only as far as
///   needed to stay unambiguous after the change. A name that is still right is not edited.
/// * Relative Markdown links, images and reference definitions get the new relative path
///   (`./` prefix, `<…>` form and `#fragment` are kept; characters that need it are
///   percent-encoded).
/// * Links of the renamed document itself are treated like any other document's.
pub fn rename_edits(
    text: &str,
    doc: &Document,
    doc_path: &RelPath,
    files: &[RelPath],
    old_path: &RelPath,
    new_path: &RelPath,
) -> Vec<Edit> {
    let mut files_after: Vec<RelPath> = files
        .iter()
        .map(|f| {
            if same_file(f, old_path) {
                new_path.clone()
            } else {
                f.clone()
            }
        })
        .collect();
    if !files_after.contains(new_path) {
        files_after.push(new_path.clone());
    }
    let mut edits = Vec::new();
    for site in sites(doc) {
        let edit = match &site.target {
            LinkTarget::Wiki { name, .. } if !name.is_empty() => {
                let points_at_old =
                    resolve_wikilink(files, name).is_some_and(|f| same_file(f, old_path));
                points_at_old
                    .then(|| renamed_wikilink(name, old_path, new_path, &files_after))
                    .filter(|renamed| renamed != name)
                    .map(|renamed| Edit::replace(trimmed_range(text, site.dest_range), renamed))
            }
            LinkTarget::File { path, .. } if !path.is_empty() => resolve_relative(doc_path, path)
                .ok()
                .filter(|resolved| same_file(resolved, old_path))
                .and_then(|_| renamed_file_destination(text, site.dest_range, doc_path, new_path)),
            _ => None,
        };
        edits.extend(edit);
    }
    edits.sort_by_key(|e| e.range.start);
    edits
}

/// `range` without the surrounding whitespace of its text.
fn trimmed_range(text: &str, range: TextRange) -> TextRange {
    let raw = range.get(text).unwrap_or("");
    let start = range.start + (raw.len() - raw.trim_start().len());
    TextRange::new(start, start + raw.trim().len())
}

/// The wikilink name for `new` that replaces `name`, which resolved to `old`.
fn renamed_wikilink(name: &str, old: &RelPath, new: &RelPath, files_after: &[RelPath]) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let with_extension = eq_ordinal_ignore_case(last, old.file_name());
    let segments = name.split(['/', '\\']).filter(|s| !s.is_empty()).count();
    shortest_name(files_after, new, with_extension, segments)
}

/// The shortest `[[name]]` that resolves to `path` within `files` (which contains it), using
/// at least `min_segments` trailing segments of the path.
fn shortest_name(
    files: &[RelPath],
    path: &RelPath,
    with_extension: bool,
    min_segments: usize,
) -> String {
    let segments: Vec<&str> = path.segments().collect();
    let extension_len = path.extension().map_or(0, |ext| ext.len() + 1);
    let name_of = |count: usize| {
        let mut name = segments[segments.len() - count..].join("/");
        if !with_extension {
            name.truncate(name.len() - extension_len);
        }
        name
    };
    for count in min_segments.clamp(1, segments.len())..segments.len() {
        let candidate = name_of(count);
        if resolve_wikilink(files, &candidate) == Some(path) {
            return candidate;
        }
    }
    name_of(segments.len())
}

/// The edit that points the destination at `range` (the path part, before any `#fragment`)
/// at `new_path`; `None` when the text would not change.
fn renamed_file_destination(
    text: &str,
    range: TextRange,
    doc_path: &RelPath,
    new_path: &RelPath,
) -> Option<Edit> {
    let raw = range.get(text).unwrap_or("");
    let path_len = raw.find('#').unwrap_or(raw.len());
    let angle = text[..range.start].ends_with('<');
    let mut relative = relative_path(doc_path.parent_dir(), new_path);
    if raw.starts_with("./") && !relative.starts_with("../") {
        relative.insert_str(0, "./");
    }
    let encoded = encode_destination(&relative, angle);
    (encoded != raw[..path_len])
        .then(|| Edit::replace(TextRange::new(range.start, range.start + path_len), encoded))
}

// ------------------------------------------------------------------------ backlinks

/// The links of one document, as produced by [`extract_links`].
#[derive(Clone, Copy, Debug)]
pub struct DocLinks<'a> {
    pub path: &'a RelPath,
    pub links: &'a [LinkRef],
}

/// A link that points at some file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backlink {
    /// The document containing the link.
    pub source: RelPath,
    /// The link node within `source`.
    pub range: TextRange,
}

/// Links in `docs` that point at `target`: wikilinks resolving to it within `files`, and
/// relative Markdown links and images resolving to it. Links in `target` itself, external
/// links and same-document anchors are ignored. Sorted by source path (ordinal, ignoring
/// case), then position.
pub fn backlinks(target: &RelPath, files: &[RelPath], docs: &[DocLinks<'_>]) -> Vec<Backlink> {
    let mut found = Vec::new();
    for doc in docs.iter().filter(|d| !same_file(d.path, target)) {
        for link in doc.links {
            let points_at_target = match &link.target {
                LinkTarget::Wiki { name, .. } => {
                    resolve_wikilink(files, name).is_some_and(|f| same_file(f, target))
                }
                LinkTarget::File { path, .. } => resolve_relative(doc.path, path)
                    .is_ok_and(|resolved| same_file(&resolved, target)),
                LinkTarget::External(_) => false,
            };
            if points_at_target {
                found.push(Backlink {
                    source: doc.path.clone(),
                    range: link.range,
                });
            }
        }
    }
    found.sort_by(|a, b| {
        cmp_ordinal_ignore_case(a.source.as_str(), b.source.as_str())
            .then_with(|| a.range.start.cmp(&b.range.start))
    });
    found
}

// ------------------------------------------------------------------------ completion

/// A `[[` completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Index into the `files` that were searched.
    pub file: usize,
    /// What to insert between `[[` and `]]`: the shortest unambiguous name without extension.
    pub insert: String,
    pub score: u32,
    /// Matched `char` indices in the file's path, for highlighting.
    pub indices: Vec<usize>,
}

/// `[[` completion: fuzzy matches of `query` over the Markdown documents in `files`, best
/// first, at most `limit`. `recent` lists indices into `files`, most recent first.
pub fn complete_wikilink(
    matcher: &mut FuzzyMatcher,
    query: &str,
    files: &[RelPath],
    recent: &[usize],
    limit: usize,
) -> Vec<Completion> {
    let documents: Vec<usize> = (0..files.len())
        .filter(|&i| files[i].is_markdown())
        .collect();
    let paths: Vec<&str> = documents.iter().map(|&i| files[i].as_str()).collect();
    let position: HashMap<usize, usize> =
        documents.iter().enumerate().map(|(p, &i)| (i, p)).collect();
    let recent: Vec<usize> = recent
        .iter()
        .filter_map(|i| position.get(i).copied())
        .collect();
    matcher
        .rank_paths(query, &paths, &recent, limit)
        .into_iter()
        .map(|ranked| {
            let file = documents[ranked.index];
            Completion {
                file,
                insert: shortest_name(files, &files[file], false, 1),
                score: ranked.score,
                indices: ranked.indices,
            }
        })
        .collect()
}

/// A `[[#` completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadingCompletion {
    /// Index into [`Toc::entries`].
    pub heading: usize,
    /// What to insert after `[[#`: the title without the characters that would break the
    /// wikilink (`#`, `|`, `[`, `]` become spaces).
    pub insert: String,
    pub score: u32,
    /// Matched `char` indices in the heading title.
    pub indices: Vec<usize>,
}

/// `[[#` completion: fuzzy matches of `query` over the headings of `toc`, best first, at most
/// `limit`.
pub fn complete_heading(
    matcher: &mut FuzzyMatcher,
    query: &str,
    toc: &Toc,
    limit: usize,
) -> Vec<HeadingCompletion> {
    let titles: Vec<&str> = toc.entries().iter().map(|e| e.title.as_str()).collect();
    matcher
        .rank_texts(query, &titles, limit)
        .into_iter()
        .map(|ranked| {
            let title = titles[ranked.index].replace(['#', '|', '[', ']'], " ");
            HeadingCompletion {
                heading: ranked.index,
                insert: title.split_whitespace().collect::<Vec<_>>().join(" "),
                score: ranked.score,
                indices: ranked.indices,
            }
        })
        .collect()
}

// ------------------------------------------------------------------------ absolute roots

/// Where an absolute path lives.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Prefix {
    Relative,
    /// Upper-case drive letter.
    Drive(char),
    /// Simply uppercased server and share.
    Unc {
        server: String,
        share: String,
    },
}

struct WinPath {
    prefix: Prefix,
    /// Segments without empty and `.` ones; `..` is kept.
    segments: Vec<String>,
}

/// Lexical parse of a Windows path (`C:\a`, `\\server\share\a`, `\\?\C:\a`, `\\?\UNC\s\sh\a`,
/// relative `a\b`; `/` is accepted for `\`). `None` for drive-relative (`C:a`), rooted without
/// a drive (`\a`), device-namespace (`\\.\`) and malformed paths.
fn parse_win_path(path: &str) -> Option<WinPath> {
    if path.is_empty() {
        return None;
    }
    let norm = path.replace('/', "\\");
    if norm.starts_with(r"\\.\") {
        return None;
    }
    let upper = |s: &str| s.chars().map(simple_upper).collect::<String>();
    let mut rest = norm.as_str();
    let verbatim = rest.starts_with(r"\\?\");
    if verbatim {
        rest = &rest[4..];
    }
    let unc = if verbatim {
        rest.get(..4)
            .filter(|p| p.eq_ignore_ascii_case(r"UNC\"))
            .map(|_| &rest[4..])
    } else {
        rest.strip_prefix(r"\\")
    };
    let (prefix, body) = if let Some(after) = unc {
        let mut parts = after.splitn(3, '\\');
        let server = parts
            .next()
            .filter(|s| !s.is_empty() && *s != "." && *s != "..")?;
        let share = parts
            .next()
            .filter(|s| !s.is_empty() && *s != "." && *s != "..")?;
        (
            Prefix::Unc {
                server: upper(server),
                share: upper(share),
            },
            parts.next().unwrap_or(""),
        )
    } else {
        match rest.as_bytes() {
            [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic() => (
                Prefix::Drive(drive.to_ascii_uppercase() as char),
                &rest[3..],
            ),
            [drive, b':', ..] if drive.is_ascii_alphabetic() => return None,
            [b'\\', ..] => return None,
            _ if verbatim => return None,
            _ => (Prefix::Relative, rest),
        }
    };
    let mut segments = Vec::new();
    for segment in body.split('\\') {
        match segment {
            "" | "." => {}
            ".." => segments.push("..".to_owned()),
            name if segment_is_valid(name) => segments.push(name.to_owned()),
            _ => return None,
        }
    }
    Some(WinPath { prefix, segments })
}

/// Appends `segments` to `stack`, resolving `..`; `false` when a `..` would pop below `floor`
/// entries.
fn collapse(stack: &mut Vec<String>, segments: Vec<String>, floor: usize) -> bool {
    for segment in segments {
        if segment == ".." {
            if stack.len() <= floor {
                return false;
            }
            stack.pop();
        } else {
            stack.push(segment);
        }
    }
    true
}

/// Whether `candidate` is `root` or lies below it, judged lexically on Windows paths: separators
/// and case are normalized (ordinal, ignoring case), `.` and `..` are resolved. `candidate` may
/// be relative (taken relative to `root`) or absolute. Refused (`false`): any `..` that would
/// leave the root (relative candidates may not even climb into the root's own name), another
/// drive or UNC share, drive-relative (`C:x`), rooted-without-drive (`\x`) and device
/// (`\\.\`) paths, malformed names, and a `root` that is not absolute.
pub fn is_inside_root(root: &str, candidate: &str) -> bool {
    let (Some(root), Some(candidate)) = (parse_win_path(root), parse_win_path(candidate)) else {
        return false;
    };
    if root.prefix == Prefix::Relative {
        return false;
    }
    let mut base: Vec<String> = Vec::new();
    if !collapse(&mut base, root.segments, 0) {
        return false;
    }
    let (prefix, mut stack, floor) = match candidate.prefix {
        Prefix::Relative => (root.prefix.clone(), base.clone(), base.len()),
        absolute => (absolute, Vec::new(), 0),
    };
    if !collapse(&mut stack, candidate.segments, floor) {
        return false;
    }
    prefix == root.prefix
        && stack.len() >= base.len()
        && base
            .iter()
            .zip(&stack)
            .all(|(a, b)| eq_ordinal_ignore_case(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse_document;

    fn rel(path: &str) -> RelPath {
        RelPath::parse(path).unwrap()
    }

    fn rels(paths: &[&str]) -> Vec<RelPath> {
        paths.iter().map(|p| rel(p)).collect()
    }

    fn apply(text: &str, edits: &[Edit]) -> String {
        let mut out = String::new();
        let mut pos = 0;
        for e in edits {
            assert!(
                e.range.start >= pos,
                "edits must be sorted and disjoint: {edits:?}"
            );
            out.push_str(&text[pos..e.range.start]);
            out.push_str(&e.text);
            pos = e.range.end;
        }
        out.push_str(&text[pos..]);
        out
    }

    /// Applies the rename edits of `text` (a document at `doc`) and returns the new text.
    fn renamed(text: &str, doc: &str, files: &[&str], old: &str, new: &str) -> String {
        let ast = parse_document(text);
        let edits = rename_edits(text, &ast, &rel(doc), &rels(files), &rel(old), &rel(new));
        apply(text, &edits)
    }

    // -------------------------------------------------------------------- RelPath

    #[test]
    fn parse_normalizes_separators_dots_and_parents() {
        for (input, expected) in [
            ("a.md", "a.md"),
            ("./a/./b.md", "a/b.md"),
            ("a\\b\\c.md", "a/b/c.md"),
            ("a//b", "a/b"),
            ("a/../b.md", "b.md"),
            ("a/b/../../c.md", "c.md"),
            ("日本語/ファイル.md", "日本語/ファイル.md"),
            ("console.md", "console.md"),
            ("com10.md", "com10.md"),
            ("comx/lpt.txt", "comx/lpt.txt"),
            (".hidden/.gitignore", ".hidden/.gitignore"),
        ] {
            assert_eq!(
                RelPath::parse(input).unwrap().as_str(),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_rejects_what_cannot_be_a_workspace_path() {
        for (input, error) in [
            ("", PathError::Empty),
            (".", PathError::Empty),
            ("a/..", PathError::Empty),
            ("./", PathError::Empty),
            ("../x", PathError::EscapesRoot),
            ("a/../../x", PathError::EscapesRoot),
            ("/x", PathError::Absolute),
            ("\\x", PathError::Absolute),
            ("C:\\x", PathError::Absolute),
            ("c:/x", PathError::Absolute),
            ("c:x", PathError::Absolute),
            ("\\\\srv\\share\\x", PathError::Absolute),
            ("//srv/share/x", PathError::Absolute),
            ("a<b.md", PathError::Invalid),
            ("a|b", PathError::Invalid),
            ("a?b", PathError::Invalid),
            ("a*b", PathError::Invalid),
            ("x/a:b", PathError::Invalid),
            ("x/\"q\"", PathError::Invalid),
            ("tab\tname", PathError::Invalid),
            ("del\u{7f}", PathError::Invalid),
            ("con.md", PathError::Invalid),
            ("x/NUL", PathError::Invalid),
            ("Prn.txt", PathError::Invalid),
            ("AUX ", PathError::Invalid),
            ("dir/Com1.txt", PathError::Invalid),
            ("lpt9", PathError::Invalid),
            ("trailing.", PathError::Invalid),
            ("a /b", PathError::Invalid),
            ("...", PathError::Invalid),
        ] {
            assert_eq!(RelPath::parse(input), Err(error), "{input:?}");
        }
        for (error, message) in [
            (PathError::Empty, "the path is empty"),
            (PathError::Absolute, "the path is absolute"),
            (PathError::EscapesRoot, "the path leaves the workspace root"),
            (
                PathError::Invalid,
                "the path contains a name or character that Windows does not allow",
            ),
        ] {
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn rel_path_parts() {
        let p = rel("notes/sub/a.b.md");
        assert_eq!(p.file_name(), "a.b.md");
        assert_eq!(p.parent_dir(), "notes/sub");
        assert_eq!(p.extension(), Some("md"));
        assert_eq!(p.without_extension(), "notes/sub/a.b");
        assert_eq!(p.segments().collect::<Vec<_>>(), ["notes", "sub", "a.b.md"]);
        assert!(p.is_markdown());
        let root_file = rel("README.MARKDOWN");
        assert_eq!(root_file.parent_dir(), "");
        assert_eq!(root_file.file_name(), "README.MARKDOWN");
        assert!(root_file.is_markdown());
        for (path, ext) in [
            (".gitignore", None),
            ("dir/noext", None),
            ("x/.hidden.md", Some("md")),
        ] {
            let p = rel(path);
            assert_eq!(p.extension(), ext, "{path}");
            if ext.is_none() {
                assert_eq!(p.without_extension(), path);
                assert!(!p.is_markdown());
            }
        }
        assert!(!rel("a.txt").is_markdown());
        assert!(rel("a.mdx").is_markdown());
    }

    // ------------------------------------------------------------ ordinal compare

    #[test]
    fn ordinal_comparison_uses_simple_uppercase() {
        assert!(eq_ordinal_ignore_case("README.md", "readme.MD"));
        assert!(eq_ordinal_ignore_case("é", "É"));
        assert!(eq_ordinal_ignore_case("ǆ", "Ǆ"));
        assert!(eq_ordinal_ignore_case("日本", "日本"));
        assert!(eq_ordinal_ignore_case("ıs", "IS"));
        assert!(!eq_ordinal_ignore_case("ß", "SS"));
        assert!(!eq_ordinal_ignore_case("İ", "i"));
        assert!(!eq_ordinal_ignore_case("a", "ab"));
        assert_eq!(cmp_ordinal_ignore_case("a", "B"), Ordering::Less);
        assert_eq!(cmp_ordinal_ignore_case("B", "a"), Ordering::Greater);
        assert_eq!(cmp_ordinal_ignore_case("Ab", "aB"), Ordering::Equal);
        assert_eq!(cmp_ordinal_ignore_case("a", "ab"), Ordering::Less);
        // UTF-16 order: an astral character (surrogates D83D…) sorts before U+FF5E.
        assert_eq!(cmp_ordinal_ignore_case("😀", "\u{FF5E}"), Ordering::Less);
    }

    // ---------------------------------------------------------- percent encoding

    #[test]
    fn percent_decoding() {
        for (input, expected) in [
            ("a%20b", "a b"),
            ("%E6%97%A5%E6%9C%AC", "日本"),
            ("a%2fb%2Fc", "a/b/c"),
            ("plain", "plain"),
            ("%zz", "%zz"),
            ("%4", "%4"),
            ("100%", "100%"),
            ("%", "%"),
            ("%FF", "%FF"),
            ("%C3%28", "%C3%28"),
            ("50%25 off", "50% off"),
        ] {
            assert_eq!(percent_decode(input), expected, "{input:?}");
        }
    }

    #[test]
    fn destination_encoding() {
        assert_eq!(
            encode_destination("a b(1)#x%.md", false),
            "a%20b%281%29%23x%25.md"
        );
        assert_eq!(encode_destination("a b(1)#x%.md", true), "a b(1)%23x%25.md");
        assert_eq!(encode_destination("日本/é.md", false), "日本/é.md");
        assert_eq!(encode_destination("<a>\n.md", true), "%3Ca%3E%0A.md");
        assert_eq!(
            percent_decode(&encode_destination("a b/ü (1)#.md", false)),
            "a b/ü (1)#.md"
        );
    }

    // ------------------------------------------------------------- classification

    #[test]
    fn destinations_are_classified() {
        let file = |path: &str, fragment: Option<&str>| LinkTarget::File {
            path: path.to_owned(),
            fragment: fragment.map(str::to_owned),
        };
        for (dest, expected) in [
            (
                "https://x.org/a#b",
                LinkTarget::External("https://x.org/a#b".into()),
            ),
            (
                "mailto:me@x.org",
                LinkTarget::External("mailto:me@x.org".into()),
            ),
            ("//cdn.x/y.js", LinkTarget::External("//cdn.x/y.js".into())),
            ("a+b.c-d:rest", LinkTarget::External("a+b.c-d:rest".into())),
            ("C:\\x\\a.md", file("C:\\x\\a.md", None)),
            ("x:y", file("x:y", None)),
            ("1http:x", file("1http:x", None)),
            ("ht tp:x", file("ht tp:x", None)),
            ("a%20b.md#Sec%20Tion", file("a b.md", Some("Sec Tion"))),
            ("#frag", file("", Some("frag"))),
            ("", file("", None)),
            ("../a.md#", file("../a.md", Some(""))),
        ] {
            assert_eq!(classify_destination(dest), expected, "{dest:?}");
        }
    }

    #[test]
    fn links_images_and_wikilinks_are_extracted_with_targets() {
        let src = "# See [[Head Note]]\n\n\
                   [inline](a%20b.md#Sec) and ![img](pics/x.png \"t\") and [ext](https://e.org)\n\n\
                   [ref][r] [[Note#Part|alias]] [[#Local]] [self](#top) <https://auto.org>\n\n\
                   | cell |\n|---|\n| [[In Table]] [t](t.md) |\n\n\
                   <img src=\"html.png\">\n\n\
                   [![nested](n.png)](outer.md)\n\n\
                   [r]: ref.md\n";
        let links = extract_links(&parse_document(src));
        let summary: Vec<(LinkSource, String)> = links
            .iter()
            .map(|l| {
                let target = match &l.target {
                    LinkTarget::External(u) => format!("ext:{u}"),
                    LinkTarget::File { path, fragment } => format!("file:{path}#{fragment:?}"),
                    LinkTarget::Wiki { name, heading } => format!("wiki:{name}#{heading:?}"),
                };
                (l.source, target)
            })
            .collect();
        assert_eq!(
            summary,
            [
                (LinkSource::Wiki, "wiki:Head Note#None".to_owned()),
                (LinkSource::Link, "file:a b.md#Some(\"Sec\")".to_owned()),
                (LinkSource::Image, "file:pics/x.png#None".to_owned()),
                (LinkSource::Link, "ext:https://e.org".to_owned()),
                (LinkSource::Link, "file:ref.md#None".to_owned()),
                (LinkSource::Wiki, "wiki:Note#Some(\"Part\")".to_owned()),
                (LinkSource::Wiki, "wiki:#Some(\"Local\")".to_owned()),
                (LinkSource::Link, "file:#Some(\"top\")".to_owned()),
                (LinkSource::Link, "ext:https://auto.org".to_owned()),
                (LinkSource::Wiki, "wiki:In Table#None".to_owned()),
                (LinkSource::Link, "file:t.md#None".to_owned()),
                (LinkSource::Image, "file:html.png#None".to_owned()),
                (LinkSource::Link, "file:outer.md#None".to_owned()),
                (LinkSource::Image, "file:n.png#None".to_owned()),
            ]
        );
        let at = |i: usize| links[i].dest_range.map(|r| &src[r.start..r.end]);
        assert_eq!(at(0), Some("Head Note"));
        assert_eq!(at(1), Some("a%20b.md#Sec"));
        assert_eq!(at(2), Some("pics/x.png"));
        assert_eq!(at(4), None); // reference-style use
        assert_eq!(at(5), Some("Note"));
        assert_eq!(at(8), None); // autolink
        assert_eq!(at(11), None); // <img>
        assert_eq!(at(12), Some("outer.md"));
        assert_eq!(
            &src[links[1].range.start..links[1].range.end],
            "[inline](a%20b.md#Sec)"
        );
    }

    // ------------------------------------------------------------------ resolution

    #[test]
    fn wikilink_resolution_tiers() {
        let files = rels(&[
            "a.md",
            "notes/a.md",
            "notes/deep/a.md",
            "Notes/B.md",
            "docs/guide.md",
            "docs/Guide.markdown",
            "img/a.png",
            "x/v1.2.md",
        ]);
        let resolve = |name: &str| resolve_wikilink(&files, name).map(RelPath::as_str);
        assert_eq!(resolve("a"), Some("a.md")); // stem anywhere, shortest wins
        assert_eq!(resolve("notes/a"), Some("notes/a.md")); // path without extension
        assert_eq!(resolve("notes\\a"), Some("notes/a.md"));
        assert_eq!(resolve("notes/a.md"), Some("notes/a.md")); // exact path
        assert_eq!(resolve("NOTES/A.MD"), Some("notes/a.md")); // ignoring case
        assert_eq!(resolve("deep/a"), Some("notes/deep/a.md")); // trailing part
        assert_eq!(resolve("b"), Some("Notes/B.md"));
        assert_eq!(resolve("  guide  "), Some("docs/guide.md"));
        assert_eq!(resolve("v1.2"), Some("x/v1.2.md"));
        assert_eq!(resolve("img/a.png"), Some("img/a.png"));
        assert_eq!(resolve("a.png"), Some("img/a.png")); // trailing part with extension
        assert_eq!(resolve("otes/a"), None); // segment-aligned
        assert_eq!(resolve("missing"), None);
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("   "), None);
        assert_eq!(resolve("../a"), None);
        // Without the root file, `a.md` is a trailing part of two others: the shortest wins.
        let nested = rels(&["notes/deep/a.md", "notes/a.md"]);
        assert_eq!(
            resolve_wikilink(&nested, "a.md").map(RelPath::as_str),
            Some("notes/a.md")
        );
    }

    #[test]
    fn wikilink_ties_are_alphabetical_then_exact() {
        let tie = rels(&["y/a.md", "x/a.md"]);
        assert_eq!(resolve_wikilink(&tie, "a").unwrap().as_str(), "x/a.md");
        let mixed = rels(&["B/a.md", "a/a.md"]);
        assert_eq!(resolve_wikilink(&mixed, "a").unwrap().as_str(), "a/a.md");
        let same_ignoring_case = rels(&["x/a.md", "X/a.md"]);
        assert_eq!(
            resolve_wikilink(&same_ignoring_case, "A").unwrap().as_str(),
            "X/a.md"
        );
        // Shorter in characters, not in bytes: `ı` takes two bytes.
        let chars = rels(&["ııı/a.md", "abcd/a.md"]);
        assert_eq!(resolve_wikilink(&chars, "a").unwrap().as_str(), "ııı/a.md");
        // A better tier beats a shorter path.
        let tiers = rels(&["q/x", "x.md"]);
        assert_eq!(resolve_wikilink(&tiers, "x").unwrap().as_str(), "x.md");
    }

    #[test]
    fn wikilink_resolution_follows_ordinal_not_turkish_rules() {
        let files = rels(&["İstanbul.md", "ıstakoz.md"]);
        assert_eq!(
            resolve_wikilink(&files, "İSTANBUL").unwrap().as_str(),
            "İstanbul.md"
        );
        assert_eq!(resolve_wikilink(&files, "istanbul"), None);
        assert_eq!(
            resolve_wikilink(&files, "ISTAKOZ").unwrap().as_str(),
            "ıstakoz.md"
        );
    }

    #[test]
    fn headings_resolve_by_title_then_anchor() {
        let src = "# Intro\n## Sub Section\n## Sub Section\n# İstanbul\n# Tab\tbed\n";
        let toc = Toc::build(&parse_document(src), src);
        let find = |wanted: &str| find_heading(&toc, wanted);
        assert_eq!(find("Intro"), Some(0));
        assert_eq!(find("intro"), Some(0));
        assert_eq!(find("  sub   SECTION "), Some(1));
        assert_eq!(find("sub-section"), Some(1));
        assert_eq!(find("sub-section-1"), Some(2));
        assert_eq!(find("istanbul"), Some(3));
        assert_eq!(find("İSTANBUL"), Some(3));
        assert_eq!(find("tab bed"), Some(4));
        assert_eq!(find("Missing"), None);
        assert_eq!(find(""), None);
    }

    #[test]
    fn relative_links_resolve_against_the_document() {
        let doc = rel("docs/sub/a.md");
        let ok = |link: &str| resolve_relative(&doc, link).map(|p| p.as_str().to_owned());
        assert_eq!(ok("b.md").as_deref(), Ok("docs/sub/b.md"));
        assert_eq!(ok("./b.md").as_deref(), Ok("docs/sub/b.md"));
        assert_eq!(ok("../x.md").as_deref(), Ok("docs/x.md"));
        assert_eq!(ok("../../y.md").as_deref(), Ok("y.md"));
        assert_eq!(ok("c/./d/../e.md").as_deref(), Ok("docs/sub/c/e.md"));
        assert_eq!(ok("c\\e.md").as_deref(), Ok("docs/sub/c/e.md"));
        assert_eq!(ok("../../../z.md"), Err(PathError::EscapesRoot));
        assert_eq!(ok("/abs.md"), Err(PathError::Absolute));
        assert_eq!(ok("C:/x.md"), Err(PathError::Absolute));
        assert_eq!(ok("\\\\srv\\share\\x"), Err(PathError::Absolute));
        assert_eq!(ok(""), Err(PathError::Empty));
        assert_eq!(ok("x|y.md"), Err(PathError::Invalid));
        let root_doc = rel("a.md");
        assert_eq!(
            resolve_relative(&root_doc, "../x").unwrap_err(),
            PathError::EscapesRoot
        );
        assert_eq!(
            resolve_relative(&root_doc, "sub/b.md").unwrap().as_str(),
            "sub/b.md"
        );
    }

    #[test]
    fn relative_paths_between_folders() {
        for (from, to, expected) in [
            ("", "a.md", "a.md"),
            ("docs", "docs/a.md", "a.md"),
            ("docs", "docs/sub/b.md", "sub/b.md"),
            ("docs/x", "docs/a.md", "../a.md"),
            ("docs/x/y", "a.md", "../../../a.md"),
            ("notes", "docs/a.md", "../docs/a.md"),
            ("Docs", "docs/a.md", "a.md"),
            ("", "d/e/f.md", "d/e/f.md"),
        ] {
            assert_eq!(relative_path(from, &rel(to)), expected, "{from} -> {to}");
        }
    }

    #[test]
    fn new_note_paths() {
        for (name, expected) in [
            ("note", "note.md"),
            ("  spaced  ", "spaced.md"),
            ("dir/new note", "dir/new note.md"),
            ("a.md", "a.md"),
            ("a.MARKDOWN", "a.MARKDOWN"),
            ("a.txt", "a.txt"),
            ("a.mmd", "a.mmd"),
            ("img.png", "img.png.md"),
            ("v1.2", "v1.2.md"),
            ("x\\y", "x/y.md"),
        ] {
            assert_eq!(
                wikilink_new_file(name).unwrap().as_str(),
                expected,
                "{name:?}"
            );
        }
        assert_eq!(wikilink_new_file("../x"), Err(PathError::EscapesRoot));
        assert_eq!(wikilink_new_file("a/../../x"), Err(PathError::EscapesRoot));
        assert_eq!(wikilink_new_file("C:/x"), Err(PathError::Absolute));
        assert_eq!(wikilink_new_file("/x"), Err(PathError::Absolute));
        assert_eq!(wikilink_new_file(""), Err(PathError::Empty));
        assert_eq!(wikilink_new_file("a|b"), Err(PathError::Invalid));
        assert_eq!(wikilink_new_file("con"), Err(PathError::Invalid));
    }

    // --------------------------------------------------------------------- rename

    #[test]
    fn rename_wikilinks_keep_alias_heading_and_extension_style() {
        let text =
            "[[a]] [[a|alias]] [[a#Head]] [[a#Head|alias]] [[a.md]] [[ a ]] [[A]] [[other]] [[#a]]";
        let out = renamed(text, "c.md", &["a.md", "c.md", "other.md"], "a.md", "b.md");
        assert_eq!(
            out,
            "[[b]] [[b|alias]] [[b#Head]] [[b#Head|alias]] [[b.md]] [[ b ]] [[b]] [[other]] [[#a]]"
        );
    }

    #[test]
    fn rename_wikilinks_stay_short_unless_ambiguous() {
        // Moving into a folder keeps `[[a]]` valid: nothing to edit.
        let files = ["a.md", "c.md"];
        assert_eq!(
            renamed("[[a]] [[a.md]]", "c.md", &files, "a.md", "sub/a.md"),
            "[[a]] [[a.md]]"
        );
        // A same-named file elsewhere makes the short name ambiguous: add a folder.
        let files = ["a.md", "sub/a.md", "c.md"];
        assert_eq!(
            renamed("[[a]]", "c.md", &files, "a.md", "other/a.md"),
            "[[other/a]]"
        );
        // `[[a]]` resolved to the root file; the file in `sub` is unaffected.
        assert_eq!(
            renamed("[[a]] [[sub/a]]", "c.md", &files, "a.md", "b.md"),
            "[[b]] [[sub/a]]"
        );
    }

    #[test]
    fn rename_wikilinks_with_folders() {
        let files = ["notes/a.md", "c.md"];
        let text = "[[notes/a]] [[a]] [[notes/a.md]] [[NOTES/A#h]]";
        assert_eq!(
            renamed(text, "c.md", &files, "notes/a.md", "notes/b.md"),
            "[[notes/b]] [[b]] [[notes/b.md]] [[notes/b#h]]"
        );
        // Moved to the root: a name with more segments than the new path shrinks.
        assert_eq!(
            renamed("[[notes/a]]", "c.md", &files, "notes/a.md", "a.md"),
            "[[a]]"
        );
        // A pure case change is an edit too.
        assert_eq!(renamed("[[a]]", "c.md", &["a.md"], "a.md", "A.md"), "[[A]]");
    }

    #[test]
    fn rename_markdown_links() {
        let text = "[x](a.md) [y](./a.md#h) [z](../docs/a.md) ![i](a.md) [w](other.md) [e](https://x/a.md) [s](#a)";
        assert_eq!(
            renamed(text, "docs/guide.md", &[], "docs/a.md", "docs/sub/b.md"),
            "[x](sub/b.md) [y](./sub/b.md#h) [z](sub/b.md) ![i](sub/b.md) [w](other.md) [e](https://x/a.md) [s](#a)"
        );
    }

    #[test]
    fn rename_markdown_links_between_folders_and_encoding() {
        assert_eq!(
            renamed(
                "[x](../docs/a.md)",
                "notes/n.md",
                &[],
                "docs/a.md",
                "archive/a.md"
            ),
            "[x](../archive/a.md)"
        );
        assert_eq!(
            renamed(
                "[x](../docs/a.md#top)",
                "notes/n.md",
                &[],
                "docs/a.md",
                "docs/my file (1).md"
            ),
            "[x](../docs/my%20file%20%281%29.md#top)"
        );
        assert_eq!(
            renamed(
                "[x](<../docs/a.md>)",
                "notes/n.md",
                &[],
                "docs/a.md",
                "docs/my file (1).md"
            ),
            "[x](<../docs/my file (1).md>)"
        );
        assert_eq!(
            renamed(
                "[x](my%20note.md) [y](<my note.md>)",
                "n.md",
                &[],
                "my note.md",
                "n2.md"
            ),
            "[x](n2.md) [y](<n2.md>)"
        );
        assert_eq!(
            renamed("[x](A.MD) [y](./sub/../a.md)", "n.md", &[], "a.md", "b.md"),
            "[x](b.md) [y](./b.md)"
        );
        assert_eq!(
            renamed("[x](a.md)", "n.md", &[], "a.md", "dir/日本.md"),
            "[x](dir/日本.md)"
        );
        // `./` is dropped when the new path climbs out.
        assert_eq!(
            renamed("[x](./a.md)", "sub/n.md", &[], "sub/a.md", "a.md"),
            "[x](../a.md)"
        );
    }

    #[test]
    fn rename_reference_definitions_but_not_their_uses() {
        let text = "[t][ref] and [u][ref]\n\n[ref]: a.md \"Title\"\n\n[other]: <a.md#frag>\n";
        assert_eq!(
            renamed(text, "n.md", &[], "a.md", "b c.md"),
            "[t][ref] and [u][ref]\n\n[ref]: b%20c.md \"Title\"\n\n[other]: <b c.md#frag>\n"
        );
    }

    #[test]
    fn rename_ignores_unrelated_escaping_and_unchanged_links() {
        // Escapes the root, invalid, absolute and empty paths are not links to the file.
        let text = "[a](../../a.md) [b](x|y.md) [c](/a.md) [d](C:/a.md) [e]() [f](a.md#x)";
        assert_eq!(
            renamed(text, "n.md", &[], "a.md", "b.md"),
            "[a](../../a.md) [b](x|y.md) [c](/a.md) [d](C:/a.md) [e]() [f](b.md#x)"
        );
        // Renaming a file to itself changes nothing.
        let text = "[x](a.md) [[a]]";
        assert_eq!(
            renamed(text, "n.md", &["a.md", "n.md"], "a.md", "a.md"),
            text
        );
        // A wikilink that does not resolve to the old file stays.
        assert_eq!(
            renamed("[[ghost]]", "n.md", &["a.md"], "a.md", "b.md"),
            "[[ghost]]"
        );
    }

    #[test]
    fn rename_edits_are_sorted_even_for_nested_links() {
        let text =
            "[![i](a.png)](a.md) | [[a]]\n\n| [[a]] | [x](a.md) |\n|---|---|\n| [y](a.md) | z |\n";
        let ast = parse_document(text);
        let files = rels(&["a.md", "a.png", "n.md"]);
        let doc = rel("n.md");
        let edits = rename_edits(text, &ast, &doc, &files, &rel("a.md"), &rel("b.md"));
        assert!(edits.windows(2).all(|w| w[0].range.end <= w[1].range.start));
        assert_eq!(edits.len(), 5);
        assert_eq!(
            apply(text, &edits),
            "[![i](a.png)](b.md) | [[b]]\n\n| [[b]] | [x](b.md) |\n|---|---|\n| [y](b.md) | z |\n"
        );
        let image_only = rename_edits(text, &ast, &doc, &files, &rel("a.png"), &rel("img/p.png"));
        assert_eq!(
            apply(text, &image_only),
            "[![i](img/p.png)](a.md) | [[a]]\n\n| [[a]] | [x](a.md) |\n|---|---|\n| [y](a.md) | z |\n"
        );
    }

    // ------------------------------------------------------------------ backlinks

    #[test]
    fn backlinks_collect_wikilinks_and_relative_links() {
        let files = rels(&["a.md", "notes/b.md", "c.md", "d.md"]);
        let texts = [
            ("a.md", "[[a]] [[b]] self"),
            (
                "notes/b.md",
                "[x](../a.md) [[a#h]] [[#h]] [e](https://x/a.md) [bad](../../a.md)",
            ),
            ("c.md", "[[a]] [[b|x]] ![](a.md) [[ghost]]"),
            ("d.md", "nothing"),
        ];
        let parsed: Vec<(RelPath, Vec<LinkRef>)> = texts
            .iter()
            .map(|(path, text)| (rel(path), extract_links(&parse_document(text))))
            .collect();
        let docs: Vec<DocLinks<'_>> = parsed
            .iter()
            .map(|(path, links)| DocLinks { path, links })
            .collect();
        let to_a = backlinks(&rel("a.md"), &files, &docs);
        let sources: Vec<(&str, usize)> = to_a
            .iter()
            .map(|b| (b.source.as_str(), b.range.start))
            .collect();
        assert_eq!(
            sources,
            [
                ("c.md", 0),
                ("c.md", 14),
                ("notes/b.md", 0),
                ("notes/b.md", 13)
            ]
        );
        let to_b = backlinks(&rel("notes/b.md"), &files, &docs);
        assert_eq!(
            to_b.iter().map(|b| b.source.as_str()).collect::<Vec<_>>(),
            ["a.md", "c.md"]
        );
        assert!(backlinks(&rel("d.md"), &files, &docs).is_empty());
    }

    // ----------------------------------------------------------------- completion

    #[test]
    fn wikilink_completion_lists_documents_with_unambiguous_names() {
        let files = rels(&[
            "notes/Alpha.md",
            "notes/alpine.md",
            "img/a.png",
            "docs/alpha.markdown",
            "other/Alpha.md",
            "readme.txt",
        ]);
        let mut m = FuzzyMatcher::new();
        let all = complete_wikilink(&mut m, "alp", &files, &[], 10);
        let inserts: Vec<(usize, &str)> = all.iter().map(|c| (c.file, c.insert.as_str())).collect();
        assert_eq!(all.len(), 4);
        assert!(inserts.contains(&(0, "Alpha")));
        assert!(inserts.contains(&(1, "alpine")));
        assert!(inserts.contains(&(3, "docs/alpha")));
        assert!(inserts.contains(&(4, "other/Alpha")));
        // Indices highlight characters of the path.
        let alpine = all.iter().find(|c| c.file == 1).unwrap();
        assert_eq!(alpine.indices, [6, 7, 8]);
        assert!(all.iter().all(|c| c.score > 0));
        assert_eq!(complete_wikilink(&mut m, "alp", &files, &[], 2).len(), 2);
        // Empty query: recent documents first (png and txt files are not documents).
        let recent = complete_wikilink(&mut m, "", &files, &[2, 4, 5, 1], 3);
        assert_eq!(recent.iter().map(|c| c.file).collect::<Vec<_>>(), [4, 1, 0]);
        assert!(recent.iter().all(|c| c.score == 0 && c.indices.is_empty()));
    }

    #[test]
    fn heading_completion_sanitizes_titles() {
        let src = "# Intro\n## Setup [dev] #1\n## a | b\n# Usage\n";
        let toc = Toc::build(&parse_document(src), src);
        let mut m = FuzzyMatcher::new();
        let found = complete_heading(&mut m, "set", &toc, 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading, 1);
        assert_eq!(found[0].insert, "Setup dev 1");
        assert_eq!(found[0].indices, [0, 1, 2]);
        let piped = complete_heading(&mut m, "a b", &toc, 10);
        assert_eq!(piped[0].insert, "a b");
        let all = complete_heading(&mut m, "", &toc, 2);
        assert_eq!(all.iter().map(|h| h.heading).collect::<Vec<_>>(), [0, 1]);
    }

    // ------------------------------------------------------------- absolute roots

    #[test]
    fn inside_root_accepts_descendants_and_the_root_itself() {
        let root = "C:\\Users\\me\\ws";
        for candidate in [
            "C:\\Users\\me\\ws",
            "C:\\Users\\me\\ws\\",
            "C:\\Users\\me\\ws\\a.md",
            "c:/users/ME/WS/sub/a.md",
            "C:\\Users\\me\\ws\\sub\\..\\a.md",
            "C:\\Users\\me\\ws\\.\\a.md",
            "a.md",
            "sub\\deeper/a.md",
            "sub/../a.md",
            "\\\\?\\C:\\Users\\me\\ws\\a.md",
            "C:\\Users\\me\\ws\\日本\\é.md",
        ] {
            assert!(is_inside_root(root, candidate), "{candidate:?}");
        }
        assert!(is_inside_root("C:\\", "C:\\anything\\a.md"));
        assert!(is_inside_root(
            "\\\\srv\\share\\ws",
            "\\\\SRV\\Share\\WS\\a.md"
        ));
        assert!(is_inside_root(
            "\\\\srv\\share",
            "\\\\?\\UNC\\srv\\share\\a.md"
        ));
        assert!(is_inside_root("\\\\?\\C:\\ws", "C:\\ws\\a.md"));
    }

    #[test]
    fn inside_root_rejects_escapes() {
        let root = "C:\\Users\\me\\ws";
        for candidate in [
            "..\\x.md",
            "../../x",
            "sub/../../x",
            "..",
            "..\\ws\\x.md",
            "C:\\Users\\me\\ws\\..\\x.md",
            "C:\\Users\\me\\ws\\sub\\..\\..\\x",
            "C:\\Users\\me\\other\\a.md",
            "C:\\Users\\me\\wsx\\a.md",
            "C:\\Users\\me",
            "C:\\",
            "D:\\Users\\me\\ws\\a.md",
            "C:a.md",
            "C:",
            "\\a.md",
            "\\\\srv\\share\\a.md",
            "\\\\.\\C:\\Users\\me\\ws\\a.md",
            "C:\\..\\Users\\me\\ws\\a.md",
            "C:\\Users\\me\\ws\\a|b",
            "C:\\Users\\me\\ws\\CON",
            "C:\\Users\\me\\ws\\trailing.",
            "",
        ] {
            assert!(!is_inside_root(root, candidate), "{candidate:?}");
        }
        // The root has to be absolute and well-formed.
        for bad_root in [
            "ws",
            "",
            "C:",
            "C:ws",
            "\\ws",
            "\\\\.\\C:\\ws",
            "\\\\server",
            "\\\\server\\",
            "C:\\a\\..\\..",
        ] {
            assert!(!is_inside_root(bad_root, "C:\\ws\\a.md"), "{bad_root:?}");
        }
        // Another share or server is outside.
        assert!(!is_inside_root(
            "\\\\srv\\share\\ws",
            "\\\\srv\\share2\\ws\\a.md"
        ));
        assert!(!is_inside_root(
            "\\\\srv\\share\\ws",
            "\\\\srv2\\share\\ws\\a.md"
        ));
        assert!(!is_inside_root("\\\\srv\\share\\ws", "\\\\srv\\.\\ws"));
        assert!(!is_inside_root("\\\\srv\\share\\ws", "\\\\srv\\..\\ws"));
        // A UNC path needs a real server name, plain or verbatim.
        for server_less in [
            "\\\\\\share\\ws\\a.md",
            "\\\\.\\share\\ws\\a.md",
            "\\\\..\\share\\ws\\a.md",
            "\\\\?\\UNC\\\\share\\ws\\a.md",
        ] {
            assert!(
                !is_inside_root("\\\\srv\\share\\ws", server_less),
                "{server_less:?}"
            );
        }
        assert!(!is_inside_root("\\\\srv\\share\\ws", "\\\\?\\UNC\\srv"));
        assert!(!is_inside_root(
            "\\\\srv\\share\\ws",
            "\\\\?\\UN\\srv\\share\\ws"
        ));
        assert!(!is_inside_root("C:\\ws", "\\\\srv\\share\\ws\\a.md"));
    }
}
